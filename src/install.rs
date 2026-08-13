use crate::bottle;
use crate::formula::{BottleFile, Formula};
use crate::relocate;
use crate::ui;
use crate::Ctx;
use anyhow::{bail, Context, Result};
use flate2::read::GzDecoder;
use futures::stream::{self, StreamExt};
use indicatif::{MultiProgress, ProgressBar};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::IsTerminal;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

const PARALLEL_DOWNLOADS: usize = 6;
const PARALLEL_POURS: usize = 4;
const RECEIPT: &str = ".dram-receipt.json";

/// Written into each keg at pour time. `requested` separates "user asked for
/// this" from "pulled in as a dep" — the difference between what uninstall
/// autoremoves and what it keeps. `deps` is recorded so uninstall can check
/// dependents without touching the network or the index cache.
#[derive(Serialize, Deserialize, Default)]
struct Receipt {
    requested: bool,
    /// Pinned by a project lockfile — shields from autoremove without
    /// claiming the user asked for it (kept out of Dramfile dumps).
    #[serde(default)]
    protected: bool,
    #[serde(default)]
    deps: Vec<String>,
}

/// Everything pour needs to know about one keg, independent of where the
/// metadata came from (live index or lockfile).
#[derive(Clone)]
pub struct PourSpec {
    pub name: String,
    pub version: String,
    pub deps: Vec<String>,
    pub keg_only: bool,
    pub requested: bool,
    pub post_install: Vec<serde_json::Value>,
}

impl PourSpec {
    pub fn from_formula(f: &Formula, requested: bool) -> Self {
        PourSpec {
            name: f.name.clone(),
            version: f.keg_version(),
            deps: f.dependencies.clone(),
            keg_only: f.keg_only,
            requested,
            post_install: f.post_install_steps.clone(),
        }
    }
}

/// One unit of work for pour_set: what to pour and which bottle blob to fetch.
pub struct InstallItem {
    pub spec: PourSpec,
    pub file: BottleFile,
}

/// The shared engine, wave-scheduled: bottles download in parallel (stacked
/// bars in display order), and each keg pours the moment its own blob AND its
/// in-set deps are done — so pours overlap still-running downloads, and
/// independent DAG branches pour (and post-install) concurrently, capped at
/// PARALLEL_POURS. `depmap` (name -> version) must cover the full closure so
/// relocation can pin exact dep versions. `link_global` controls whether keg
/// bins are linked into <prefix>/bin (true for `install`, false for env
/// syncs).
pub async fn pour_set(
    ctx: &Ctx,
    items: &[InstallItem],
    depmap: &HashMap<String, String>,
    link_global: bool,
) -> Result<()> {
    println!("{}", ui::dim(&format!("dram {}", env!("CARGO_PKG_VERSION"))));

    let n = items.len();
    let width = items
        .iter()
        .map(|it| it.spec.name.chars().count() + 1 + it.spec.version.chars().count())
        .max()
        .unwrap_or(0);

    // Display model: finished formulae flush into scrollback as permanent
    // lines; live download bars sit above one sticky animated summary line.
    // When output isn't a terminal the bars are hidden, so flushed lines go
    // to plain stdout instead (mp.println is swallowed when hidden).
    let interactive = std::io::stderr().is_terminal();
    let mp = MultiProgress::new();
    let sticky = mp.add(ProgressBar::new(1));
    if interactive {
        sticky.set_style(ui::sticky_style());
        sticky.enable_steady_tick(Duration::from_millis(100));
    }
    let flush = |line: String| {
        if interactive {
            let _ = mp.println(line);
        } else {
            println!("{line}");
        }
    };

    // DAG bookkeeping: how many in-set deps each item waits on, and who to
    // wake when it finishes.
    let idx_of: HashMap<&str, usize> = items
        .iter()
        .enumerate()
        .map(|(i, it)| (it.spec.name.as_str(), i))
        .collect();
    let mut deps_left = vec![0usize; n];
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, it) in items.iter().enumerate() {
        for d in &it.spec.deps {
            if let Some(&j) = idx_of.get(d.as_str()) {
                if j != i {
                    deps_left[i] += 1;
                    children[j].push(i);
                }
            }
        }
    }

    let client = reqwest::Client::new();
    let mp_ref = &mp;
    let sticky_ref = &sticky;
    let mut downloads = stream::iter(items.iter().enumerate().map(|(i, it)| {
        let client = client.clone();
        async move {
            // Live bar exists only while downloading, inserted above the
            // sticky summary; it vanishes on completion (the permanent line
            // flushes later, when the formula is fully installed).
            let pb = mp_ref.insert_before(sticky_ref, ProgressBar::new(1));
            pb.set_style(ui::download_style());
            pb.set_message(ui::row_label("", &it.spec.name, &it.spec.version, width));
            let bytes = bottle::download(&client, &it.file, &pb).await?;
            pb.finish_and_clear();
            mp_ref.remove(&pb);
            anyhow::Ok((i, bytes))
        }
    }))
    .buffer_unordered(PARALLEL_DOWNLOADS)
    .fuse();

    let sem = Arc::new(Semaphore::new(PARALLEL_POURS));
    // Serializes writes to the shared bin/ and opt/ namespaces; extraction
    // and relocation (keg-local, the expensive part) run fully parallel.
    let link_lock = Arc::new(std::sync::Mutex::new(()));
    let depmap = Arc::new(depmap.clone());
    let mut join_set: JoinSet<Result<(usize, Vec<String>)>> = JoinSet::new();

    let spawn_pour = |i: usize,
                      bytes: Vec<u8>,
                      join_set: &mut JoinSet<Result<(usize, Vec<String>)>>| {
        let ctx = ctx.clone();
        let spec = items[i].spec.clone();
        let depmap = Arc::clone(&depmap);
        let sem = Arc::clone(&sem);
        let link_lock = Arc::clone(&link_lock);
        join_set.spawn(async move {
            let _permit = sem.acquire_owned().await.expect("semaphore closed");
            let notes = tokio::task::spawn_blocking(move || {
                pour(&ctx, &spec, &bytes, &depmap, link_global, &link_lock)
            })
            .await??;
            Ok((i, notes))
        });
    };

    let mut blobs: Vec<Option<Vec<u8>>> = (0..n).map(|_| None).collect();
    let mut sizes = vec![0u64; n];
    let mut pouring: Vec<usize> = Vec::new();
    let mut downloads_done = 0usize;
    let mut poured_count = 0usize;

    let update_sticky = |downloads_done: usize, pouring: &[usize], poured: usize| {
        if !interactive {
            return;
        }
        let mut parts = Vec::new();
        let downloading = PARALLEL_DOWNLOADS.min(n - downloads_done);
        if downloading > 0 {
            parts.push(format!("downloading {downloading}"));
        }
        if !pouring.is_empty() {
            let names: Vec<&str> = pouring
                .iter()
                .take(3)
                .map(|&i| items[i].spec.name.as_str())
                .collect();
            let extra = pouring.len().saturating_sub(3);
            let more = if extra > 0 { format!(" +{extra}") } else { String::new() };
            parts.push(format!("pouring {}{more}", names.join(", ")));
        }
        let idle = downloads_done.saturating_sub(poured + pouring.len());
        if idle > 0 {
            parts.push(format!("{idle} idle"));
        }
        if parts.is_empty() {
            parts.push("finishing".into());
        }
        sticky.set_message(format!("{} · {poured}/{n} installed", parts.join(" · ")));
    };
    update_sticky(0, &pouring, 0);

    while poured_count < n {
        tokio::select! {
            Some(res) = downloads.next() => {
                let (i, bytes) = res?;
                downloads_done += 1;
                sizes[i] = bytes.len() as u64;
                if deps_left[i] == 0 {
                    spawn_pour(i, bytes, &mut join_set);
                    pouring.push(i);
                } else {
                    blobs[i] = Some(bytes);
                }
                update_sticky(downloads_done, &pouring, poured_count);
            }
            Some(joined) = join_set.join_next() => {
                let (i, notes) = joined??;
                poured_count += 1;
                pouring.retain(|&p| p != i);
                let it = &items[i];
                flush(ui::flushed_row(&it.spec.name, &it.spec.version, width, sizes[i]));
                if it.spec.keg_only && link_global {
                    flush(ui::dim(&format!("    · {} is keg-only, not linked", it.spec.name)));
                }
                for note in notes {
                    flush(format!("  {note}"));
                }
                for &c in &children[i] {
                    deps_left[c] -= 1;
                    if deps_left[c] == 0 {
                        if let Some(bytes) = blobs[c].take() {
                            spawn_pour(c, bytes, &mut join_set);
                            pouring.push(c);
                        }
                    }
                }
                update_sticky(downloads_done, &pouring, poured_count);
            }
            else => bail!("pour scheduling stalled (dependency cycle?)"),
        }
    }

    sticky.finish_and_clear();
    Ok(())
}

/// Download every bottle in parallel, then pour serially in dependency order.
/// The download fan-out is where dram earns its speed.
///
/// `roots` are the canonical names the user asked for; everything else in
/// the plan is displayed nested beneath the root that pulled it in.
pub async fn install_all(ctx: &Ctx, plan: &[&Formula], roots: &[String]) -> Result<()> {
    // Re-requesting something that arrived as a dep promotes it to
    // "requested" so a later autoremove won't take it away.
    for f in plan {
        if roots.contains(&f.name) {
            let keg = ctx.cellar().join(&f.name).join(f.keg_version());
            if keg.exists() {
                write_receipt(&keg, true, &f.dependencies)?;
            }
        }
    }

    let todo: Vec<&Formula> = plan
        .iter()
        .copied()
        .filter(|f| !ctx.cellar().join(&f.name).join(f.keg_version()).exists())
        .collect();

    if todo.is_empty() {
        println!("{} everything already installed", ui::CHECK);
        ensure_on_path(ctx)?;
        return Ok(());
    }

    let start = Instant::now();

    // depmap covers the whole plan (not just todo): already-installed deps
    // keep their current = index version for pinning.
    let depmap: HashMap<String, String> =
        plan.iter().map(|f| (f.name.clone(), f.keg_version())).collect();

    let mut items = Vec::new();
    for f in &todo {
        let picked = bottle::pick(f)?;
        items.push(InstallItem {
            spec: PourSpec::from_formula(f, roots.contains(&f.name)),
            file: picked.file.clone(),
        });
    }

    pour_set(ctx, &items, &depmap, true).await?;

    println!(
        "{} installed {} formula{} in {:.1}s",
        ui::CHECK,
        items.len(),
        if items.len() == 1 { "" } else { "e" },
        start.elapsed().as_secs_f32()
    );
    ensure_on_path(ctx)?;
    Ok(())
}

/// Extract a bottle tarball into the Cellar and wire it up. Returns notes
/// (post-install output, warnings) for the caller to display. Runs on a
/// blocking thread under wave scheduling; `link_lock` serializes writes to
/// the shared bin/ and opt/ namespaces across concurrent pours.
fn pour(
    ctx: &Ctx,
    spec: &PourSpec,
    tarball: &[u8],
    depmap: &HashMap<String, String>,
    link_global: bool,
    link_lock: &Mutex<()>,
) -> Result<Vec<String>> {
    // Bottle tarballs already contain the <name>/<version>/ layout.
    let mut archive = tar::Archive::new(GzDecoder::new(tarball));
    archive.set_preserve_permissions(true);
    archive
        .unpack(ctx.cellar())
        .with_context(|| format!("extracting {}", spec.name))?;

    let keg = ctx.cellar().join(&spec.name).join(&spec.version);
    if !keg.exists() {
        bail!("{}: tarball did not produce expected keg {}", spec.name, keg.display());
    }

    let wire_up = || -> Result<()> {
        // Keg-local and expensive — runs fully parallel across pours.
        relocate::relocate_keg(ctx, &keg, depmap)?;

        // Shared-namespace writes are serialized across concurrent pours.
        let _guard = link_lock.lock().unwrap();

        // opt/<name> -> keg. With version-pinned relocation this is no longer
        // load-bearing for new kegs, but pre-pinning kegs still resolve
        // through it, and it's a convenient stable path for users.
        let opt_link = ctx.opt().join(&spec.name);
        let _ = fs::remove_file(&opt_link);
        symlink(&keg, &opt_link)?;

        if link_global && !spec.keg_only {
            link_bin(ctx, &keg)?;
        }
        write_receipt(&keg, spec.requested, &spec.deps)
    };

    // A keg that failed mid-relocation must not survive looking installed —
    // the exists() check would skip it forever with broken load paths inside.
    if let Err(e) = wire_up() {
        let _ = fs::remove_dir_all(&keg);
        return Err(e);
    }

    // Post-install runs once the keg is fully wired; problems become notes,
    // not failures.
    Ok(crate::postinstall::run_steps(ctx, &spec.name, &spec.version, &spec.post_install))
}

fn write_receipt(keg: &Path, requested: bool, deps: &[String]) -> Result<()> {
    // Preserve an existing protection mark across re-pours.
    let protected = read_receipt(keg).map(|r| r.protected).unwrap_or(false);
    let receipt = Receipt { requested, protected, deps: deps.to_vec() };
    fs::write(keg.join(RECEIPT), serde_json::to_vec(&receipt)?)?;
    Ok(())
}

fn read_receipt(keg: &Path) -> Option<Receipt> {
    fs::read(keg.join(RECEIPT))
        .ok()
        .and_then(|raw| serde_json::from_slice(&raw).ok())
}

pub struct Outdated {
    pub name: String,
    pub installed: Vec<String>,
    pub latest: String,
}

/// Installed formulae whose latest index version isn't poured yet.
pub fn outdated_list(ctx: &Ctx, index: &crate::api::Index) -> Result<Vec<Outdated>> {
    let mut out = Vec::new();
    for i in scan_installed(ctx)? {
        if let Some(f) = index.get(&i.name) {
            let latest = f.keg_version();
            if !i.versions.contains(&latest) {
                out.push(Outdated { name: i.name, installed: i.versions, latest });
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Upgrade `names` (or everything outdated when empty). New versions pour
/// alongside the old; an old keg is deleted only when nothing else could
/// still pin it (version-pinned relocation means dependents reference the
/// exact old path until they're re-poured themselves — brew's revision
/// bumps make those dependents show up as outdated, so upgrade-all
/// converges). requested/protected flags carry across.
pub async fn upgrade(ctx: &Ctx, index: &crate::api::Index, names: &[String]) -> Result<()> {
    let all_out = outdated_list(ctx, index)?;
    let targets: Vec<&Outdated> = if names.is_empty() {
        all_out.iter().collect()
    } else {
        let mut v = Vec::new();
        for n in names {
            match all_out.iter().find(|o| &o.name == n) {
                Some(o) => v.push(o),
                None => println!("{}", ui::dim(&format!("  · {n}: already up to date (or not installed)"))),
            }
        }
        v
    };
    if targets.is_empty() {
        println!("{} everything up to date", ui::CHECK);
        return Ok(());
    }

    let target_names: Vec<String> = targets.iter().map(|o| o.name.clone()).collect();
    let plan = crate::resolver::resolve(index, &target_names)?;
    let depmap: HashMap<String, String> =
        plan.iter().map(|f| (f.name.clone(), f.keg_version())).collect();

    let receipts: HashMap<String, Receipt> = scan_installed(ctx)?
        .into_iter()
        .filter_map(|i| i.receipt.map(|r| (i.name, r)))
        .collect();

    let mut items = Vec::new();
    for f in &plan {
        if ctx.cellar().join(&f.name).join(f.keg_version()).exists() {
            continue;
        }
        let requested = receipts.get(&f.name).map(|r| r.requested).unwrap_or(false);
        items.push(InstallItem {
            spec: PourSpec::from_formula(f, requested),
            file: bottle::pick(f)?.file.clone(),
        });
    }

    let start = Instant::now();
    pour_set(ctx, &items, &depmap, true).await?;

    // Anything that appears as a dep in some receipt may be pinned at its
    // old version by that dependent — keep those old kegs.
    let pinned_names: HashSet<&str> = receipts
        .values()
        .flat_map(|r| r.deps.iter().map(|d| d.as_str()))
        .collect();

    let mut summary = Vec::new();
    for t in &targets {
        if receipts.get(&t.name).map(|r| r.protected).unwrap_or(false) {
            let deps = index.get(&t.name).map(|f| f.dependencies.clone()).unwrap_or_default();
            protect_keg(ctx, &t.name, &t.latest, &deps)?;
        }
        for v in &t.installed {
            if v == &t.latest {
                continue;
            }
            if pinned_names.contains(t.name.as_str()) {
                println!(
                    "{}",
                    ui::dim(&format!("  · kept {} {} (other kegs still pin it)", t.name, v))
                );
            } else {
                let _ = fs::remove_dir_all(ctx.cellar().join(&t.name).join(v));
            }
        }
        summary.push(format!("{} {} → {}", t.name, t.installed.join("/"), t.latest));
    }
    println!(
        "{} upgraded {} formula{} in {:.1}s: {}",
        ui::CHECK,
        targets.len(),
        if targets.len() == 1 { "" } else { "e" },
        start.elapsed().as_secs_f32(),
        summary.join(", ")
    );
    Ok(())
}

/// Names of formulae the user explicitly asked for (receipt requested=true),
/// i.e. the set a Dramfile dump should contain.
pub fn requested_names(ctx: &Ctx) -> Result<Vec<String>> {
    Ok(scan_installed(ctx)?
        .into_iter()
        .filter(|i| i.receipt.as_ref().is_some_and(|r| r.requested))
        .map(|i| i.name)
        .collect())
}

/// Mark a keg as lockfile-protected so autoremove can never sweep it,
/// without claiming the user explicitly requested it.
pub fn protect_keg(ctx: &Ctx, name: &str, version: &str, deps: &[String]) -> Result<()> {
    let keg = ctx.cellar().join(name).join(version);
    if keg.exists() {
        let mut receipt = read_receipt(&keg).unwrap_or_default();
        receipt.protected = true;
        receipt.deps = deps.to_vec();
        fs::write(keg.join(RECEIPT), serde_json::to_vec(&receipt)?)?;
    }
    Ok(())
}

/// Symlink keg/bin/* into <prefix>/bin. v1 links executables only — no
/// lib/include/share linking, so header-level consumers go through opt/.
fn link_bin(ctx: &Ctx, keg: &Path) -> Result<()> {
    let keg_bin = keg.join("bin");
    if !keg_bin.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(&keg_bin)? {
        let entry = entry?;
        let target = ctx.bin().join(entry.file_name());
        if let Ok(existing) = fs::read_link(&target) {
            // Replace links we own (i.e. pointing into our Cellar), never foreign files.
            if existing.starts_with(ctx.cellar()) {
                fs::remove_file(&target)?;
            } else {
                bail!("refusing to overwrite {}", target.display());
            }
        } else if target.exists() {
            bail!("refusing to overwrite non-symlink {}", target.display());
        }
        symlink(entry.path(), &target)?;
    }
    Ok(())
}

struct Installed {
    name: String,
    version: String,
    versions: Vec<String>,
    receipt: Option<Receipt>,
}

fn scan_installed(ctx: &Ctx) -> Result<Vec<Installed>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(ctx.cellar())? {
        let entry = entry?;
        if !entry.path().is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        // Multiple versions of a keg share one receipt for our purposes;
        // take the first one that parses.
        let mut versions = Vec::new();
        let mut receipt = None;
        for v in fs::read_dir(entry.path())?.filter_map(|v| v.ok()) {
            if !v.path().is_dir() {
                continue;
            }
            versions.push(v.file_name().to_string_lossy().to_string());
            if receipt.is_none() {
                receipt = read_receipt(&v.path());
            }
        }
        let version = versions.first().cloned().unwrap_or_default();
        out.push(Installed { name, version, versions, receipt });
    }
    Ok(out)
}

/// Uninstall `names`, refusing if anything still installed depends on them
/// (unless `force`), then sweep away deps no longer needed by anything.
pub fn uninstall(ctx: &Ctx, names: &[String], force: bool) -> Result<()> {
    let installed = scan_installed(ctx)?;
    let going: HashSet<&str> = names.iter().map(|s| s.as_str()).collect();

    for name in names {
        if !installed.iter().any(|i| &i.name == name) {
            bail!("{name} is not installed");
        }
        let dependents: Vec<&str> = installed
            .iter()
            .filter(|i| !going.contains(i.name.as_str()))
            .filter(|i| {
                i.receipt
                    .as_ref()
                    .is_some_and(|r| r.deps.iter().any(|d| d == name))
            })
            .map(|i| i.name.as_str())
            .collect();
        if !dependents.is_empty() {
            if force {
                eprintln!(
                    "\x1b[33m!\x1b[0m removing {name} even though {} depend{} on it — they may break",
                    dependents.join(", "),
                    if dependents.len() == 1 { "s" } else { "" }
                );
            } else {
                bail!(
                    "{name} is required by {} (use --force-remove/-f to remove anyway)",
                    dependents.join(", ")
                );
            }
        }
    }

    // Simulate the autoremove fixpoint up front so the whole removal set can
    // be displayed as one tree before anything is touched. A dep (never an
    // explicit install) is swept once nothing outside the removal set needs
    // it; loop because removing one can orphan another (libidn2 falls, then
    // libunistring).
    let mut removing: HashSet<String> = names.iter().cloned().collect();
    loop {
        let orphan = installed.iter().find(|i| {
            !removing.contains(&i.name)
                && i.receipt.as_ref().is_some_and(|r| !r.requested && !r.protected)
                && !installed.iter().any(|o| {
                    o.name != i.name
                        && !removing.contains(&o.name)
                        && o.receipt
                            .as_ref()
                            .is_some_and(|r| r.deps.iter().any(|d| d == &i.name))
                })
        });
        match orphan {
            Some(o) => {
                removing.insert(o.name.clone());
            }
            None => break,
        }
    }

    // Same tree grouping as install: each target root, with the autoremoved
    // deps it pulled in nested beneath (first root to claim a dep wins).
    let by_name: HashMap<&str, &Installed> =
        installed.iter().map(|i| (i.name.as_str(), i)).collect();
    let auto: HashSet<&str> = removing
        .iter()
        .map(|s| s.as_str())
        .filter(|n| !going.contains(n))
        .collect();
    let mut taken: HashSet<String> = names.iter().cloned().collect();
    let mut groups: Vec<(&Installed, Vec<&Installed>)> = Vec::new();
    for name in names {
        let root = by_name[name.as_str()];
        let mut deps = Vec::new();
        receipt_closure(root, &by_name, &auto, &mut taken, &mut deps);
        groups.push((root, deps));
    }
    // Orphans not reachable through a target's receipt chain (e.g. the
    // target had no receipt) still need a row.
    for i in &installed {
        if removing.contains(&i.name) && taken.insert(i.name.clone()) {
            groups.push((i, Vec::new()));
        }
    }

    let mut rows: Vec<(&Installed, &'static str)> = Vec::new();
    for (root, deps) in &groups {
        rows.push((root, ""));
        for (i, d) in deps.iter().enumerate() {
            rows.push((d, if i + 1 == deps.len() { "└─" } else { "├─" }));
        }
    }
    let width = rows
        .iter()
        .map(|(i, branch)| {
            let b = if branch.is_empty() { 0 } else { branch.chars().count() + 1 };
            b + i.name.chars().count() + 1 + i.version.chars().count()
        })
        .max()
        .unwrap_or(0);

    let start = Instant::now();
    println!("{}", ui::dim(&format!("dram {}", env!("CARGO_PKG_VERSION"))));
    for (i, branch) in &rows {
        remove_keg(ctx, &i.name)?;
        println!(
            "  {} {}",
            ui::row_label(branch, &i.name, &i.version, width),
            ui::CHECK
        );
    }
    println!(
        "{} uninstalled {} formula{} in {:.1}s",
        ui::CHECK,
        rows.len(),
        if rows.len() == 1 { "" } else { "e" },
        start.elapsed().as_secs_f32()
    );
    Ok(())
}

/// Post-order DFS over receipt-recorded deps, collecting those in the
/// autoremove set that haven't been claimed by an earlier root.
fn receipt_closure<'a>(
    root: &'a Installed,
    by_name: &HashMap<&str, &'a Installed>,
    auto: &HashSet<&str>,
    taken: &mut HashSet<String>,
    out: &mut Vec<&'a Installed>,
) {
    let Some(r) = &root.receipt else { return };
    for dep in &r.deps {
        if !auto.contains(dep.as_str()) {
            continue;
        }
        if let Some(&d) = by_name.get(dep.as_str()) {
            if taken.insert(d.name.clone()) {
                receipt_closure(d, by_name, auto, taken, out);
                out.push(d);
            }
        }
    }
}

fn remove_keg(ctx: &Ctx, name: &str) -> Result<()> {
    let keg_root = ctx.cellar().join(name);
    // Drop bin symlinks pointing into this keg.
    for entry in fs::read_dir(ctx.bin())? {
        let entry = entry?;
        if let Ok(dest) = fs::read_link(entry.path()) {
            if dest.starts_with(&keg_root) {
                fs::remove_file(entry.path())?;
            }
        }
    }
    let _ = fs::remove_file(ctx.opt().join(name));
    fs::remove_dir_all(&keg_root)?;
    Ok(())
}

/// Make sure <prefix>/bin is on PATH: if it isn't, append an export line to
/// the shell profile (idempotently) and tell the user once.
fn ensure_on_path(ctx: &Ctx) -> Result<()> {
    let bin = ctx.bin();
    if let Some(path) = std::env::var_os("PATH") {
        if std::env::split_paths(&path).any(|p| p == bin) {
            return Ok(());
        }
    }

    let home = std::env::var("HOME").context("HOME not set")?;
    let shell = std::env::var("SHELL").unwrap_or_default();
    let rc = if shell.ends_with("/zsh") {
        format!("{home}/.zshrc")
    } else if shell.ends_with("/bash") {
        format!("{home}/.bash_profile")
    } else {
        println!(
            "{} add {} to your PATH to use installed formulae",
            ui::dim("·"),
            bin.display()
        );
        return Ok(());
    };

    // Write $HOME-relative so the line survives a username change.
    let bin_str = bin.to_string_lossy().replace(&home, "$HOME");
    let existing = fs::read_to_string(&rc).unwrap_or_default();
    if existing.contains(&bin_str) {
        // Already configured; the current shell just hasn't picked it up.
        println!("{} restart your shell to pick up {}", ui::CHECK, bin.display());
        return Ok(());
    }

    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(&format!("\n# added by dram\nexport PATH=\"{bin_str}:$PATH\"\n"));
    fs::write(&rc, updated)?;
    println!(
        "{} added {} to PATH in {} — open a new shell or run: source {}",
        ui::CHECK,
        bin.display(),
        rc,
        rc
    );
    Ok(())
}
