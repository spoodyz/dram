//! Per-project environments — the thing brew architecturally can't do.
//!
//! `dram.toml` declares what a project wants; `dram lock` resolves it against
//! the live index and pins exact versions AND bottle digests into `dram.lock`;
//! `dram sync` materializes the environment. Because bottles are fetched by
//! locked URL + sha256, a sync reproduces the same environment even after the
//! formula index has moved on. Kegs land in the shared Cellar (they're
//! version-pinned and immutable thanks to versioned relocation), so ten
//! projects locking jq 1.8.2 share one keg; the environment itself is just
//! `.dram/bin` full of symlinks.

use crate::api;
use crate::bottle;
use crate::formula::BottleFile;
use crate::install::{self, InstallItem, PourSpec};
use crate::resolver;
use crate::ui;
use crate::Ctx;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

const MANIFEST: &str = "dram.toml";
const LOCKFILE: &str = "dram.lock";
const ENV_DIR: &str = ".dram";

#[derive(Serialize, Deserialize, Default)]
struct Manifest {
    #[serde(default)]
    packages: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct Lockfile {
    version: u32,
    /// Topologically ordered (deps before dependents).
    #[serde(default, rename = "package")]
    packages: Vec<LockedPackage>,
}

#[derive(Serialize, Deserialize)]
struct LockedPackage {
    name: String,
    version: String,
    /// Named in dram.toml (vs pulled in as a dep) — drives env bin linking.
    requested: bool,
    #[serde(default)]
    keg_only: bool,
    #[serde(default)]
    deps: Vec<String>,
    /// Declarative post-install steps, embedded as JSON so a sync replays
    /// them exactly as locked.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    post_install: String,
    /// Platform tag -> pinned bottle. All tags are recorded so one lockfile
    /// works across machines (arm64 laptop, x86 CI, ...).
    bottles: HashMap<String, LockedBottle>,
}

#[derive(Serialize, Deserialize)]
struct LockedBottle {
    url: String,
    sha256: String,
}

/// `dram init [names...]` — write a manifest (merging into an existing one).
pub fn init(names: &[String]) -> Result<()> {
    let path = Path::new(MANIFEST);
    let mut manifest: Manifest = match fs::read_to_string(path) {
        Ok(raw) => toml::from_str(&raw).context("parsing dram.toml")?,
        Err(_) => Manifest::default(),
    };
    for n in names {
        if !manifest.packages.contains(n) {
            manifest.packages.push(n.clone());
        }
    }
    fs::write(path, toml::to_string_pretty(&manifest)?)?;
    println!(
        "{} wrote {MANIFEST} ({})",
        ui::CHECK,
        if manifest.packages.is_empty() {
            "empty — add packages to it".to_string()
        } else {
            manifest.packages.join(", ")
        }
    );
    println!(
        "{}",
        ui::dim(&format!(
            "  · tip: add {ENV_DIR}/ to .gitignore; commit {MANIFEST} and {LOCKFILE}"
        ))
    );
    Ok(())
}

/// `dram lock` — resolve the manifest against the live index and pin it.
pub async fn lock(ctx: &Ctx) -> Result<()> {
    let manifest: Manifest = toml::from_str(
        &fs::read_to_string(MANIFEST)
            .with_context(|| format!("no {MANIFEST} here — run `dram init <packages>` first"))?,
    )
    .context("parsing dram.toml")?;
    if manifest.packages.is_empty() {
        bail!("{MANIFEST} lists no packages");
    }

    let index = api::fetch_index(ctx, false).await?;
    let plan = resolver::resolve(&index, &manifest.packages)?;
    let requested: Vec<String> = manifest
        .packages
        .iter()
        .filter_map(|n| index.get(n).map(|f| f.name.clone()))
        .collect();

    let mut packages = Vec::new();
    for f in &plan {
        let spec = f
            .bottle
            .get("stable")
            .with_context(|| format!("{} has no stable bottle", f.name))?;
        let bottles: HashMap<String, LockedBottle> = spec
            .files
            .iter()
            .map(|(tag, b)| {
                (
                    tag.clone(),
                    LockedBottle {
                        url: b.url.clone(),
                        sha256: b.sha256.clone(),
                    },
                )
            })
            .collect();
        packages.push(LockedPackage {
            name: f.name.clone(),
            version: f.keg_version(),
            requested: requested.contains(&f.name),
            keg_only: f.keg_only,
            deps: f.dependencies.clone(),
            post_install: if f.post_install_steps.is_empty() {
                String::new()
            } else {
                serde_json::to_string(&f.post_install_steps)?
            },
            bottles,
        });
    }

    let lockfile = Lockfile {
        version: 1,
        packages,
    };
    fs::write(LOCKFILE, toml::to_string_pretty(&lockfile)?)?;
    println!(
        "{} locked {} formula{} -> {LOCKFILE}",
        ui::CHECK,
        lockfile.packages.len(),
        if lockfile.packages.len() == 1 {
            ""
        } else {
            "e"
        },
    );
    Ok(())
}

/// `dram sync` — make the environment match the lockfile exactly, pouring
/// any missing kegs from their pinned bottles. Locks first if needed.
pub async fn sync(ctx: &Ctx) -> Result<PathBuf> {
    if !Path::new(LOCKFILE).exists() {
        lock(ctx).await?;
    }
    let lockfile: Lockfile = toml::from_str(
        &fs::read_to_string(LOCKFILE).with_context(|| format!("reading {LOCKFILE}"))?,
    )
    .context("parsing dram.lock")?;

    let depmap: HashMap<String, String> = lockfile
        .packages
        .iter()
        .map(|p| (p.name.clone(), p.version.clone()))
        .collect();

    let mut items = Vec::new();
    for p in &lockfile.packages {
        if ctx.cellar().join(&p.name).join(&p.version).exists() {
            continue;
        }
        let Some((_, locked)) = bottle::pick_tag(&p.bottles) else {
            bail!(
                "{}: no locked bottle for this platform (have: {})",
                p.name,
                p.bottles.keys().cloned().collect::<Vec<_>>().join(", ")
            );
        };
        items.push(InstallItem {
            spec: PourSpec {
                name: p.name.clone(),
                version: p.version.clone(),
                deps: p.deps.clone(),
                keg_only: p.keg_only,
                requested: false,
                post_install: if p.post_install.is_empty() {
                    Vec::new()
                } else {
                    serde_json::from_str(&p.post_install).unwrap_or_default()
                },
            },
            file: BottleFile {
                cellar: String::new(),
                url: locked.url.clone(),
                sha256: locked.sha256.clone(),
            },
            via: None,
        });
    }

    if !items.is_empty() {
        install::pour_set(ctx, &items, &depmap, false).await?;
    }

    // Every keg the lockfile references is shielded from autoremove — a
    // global uninstall must never pull a keg out from under a project.
    for p in &lockfile.packages {
        install::protect_keg(ctx, &p.name, &p.version, &p.deps)?;
    }

    // The environment: .dram/bin symlinks to each requested package's
    // executables at the locked version.
    let env_bin = Path::new(ENV_DIR).join("bin");
    let _ = fs::remove_dir_all(&env_bin);
    fs::create_dir_all(&env_bin)?;
    let mut linked = 0usize;
    for p in lockfile.packages.iter().filter(|p| p.requested) {
        let keg_bin = ctx.cellar().join(&p.name).join(&p.version).join("bin");
        if !keg_bin.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&keg_bin)? {
            let entry = entry?;
            symlink(entry.path(), env_bin.join(entry.file_name()))?;
            linked += 1;
        }
    }

    println!(
        "{} env synced: {} poured, {} executable{} in {}",
        ui::CHECK,
        items.len(),
        linked,
        if linked == 1 { "" } else { "s" },
        env_bin.display()
    );
    Ok(env_bin.canonicalize()?)
}

/// `dram shell` — sync, then drop into a subshell with the env on PATH.
pub async fn shell(ctx: &Ctx) -> Result<()> {
    let env_bin = sync(ctx).await?;
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let path = std::env::var("PATH").unwrap_or_default();
    println!("{}", ui::dim("  · entering dram env (exit to leave)"));

    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(&shell)
        .env("PATH", format!("{}:{path}", env_bin.display()))
        .env("DRAM_ENV", std::env::current_dir()?.as_os_str())
        .exec();
    // exec only returns on failure.
    bail!("failed to exec {shell}: {err}");
}

/// `dram env` — print shell code for eval / direnv.
pub fn print_env() -> Result<()> {
    let env_bin = std::env::current_dir()?.join(ENV_DIR).join("bin");
    println!("export PATH=\"{}:$PATH\"", env_bin.display());
    Ok(())
}
