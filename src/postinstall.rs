//! Interpreter for Homebrew's declarative post_install steps.
//!
//! The formula API exposes `post_install_steps` as typed JSON (mkdir_p,
//! symlink, run, ...) — no Ruby required. Steps run after a keg is poured,
//! linked, and receipted. Failures are reported as notes, never fatal: a
//! half-working tool the user can see beats a nuked keg.

use crate::Ctx;
use serde_json::Value;
use std::fs;
use std::os::unix::fs::symlink;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct StepCtx<'a> {
    pub ctx: &'a Ctx,
    pub name: &'a str,
    pub version: &'a str,
}

impl StepCtx<'_> {
    fn keg(&self) -> PathBuf {
        self.ctx.cellar().join(self.name).join(self.version)
    }

    /// Expand {{template}} variables.
    fn template(&self, s: &str) -> String {
        let prefix = self.ctx.prefix.to_string_lossy().to_string();
        let keg = self.keg().to_string_lossy().to_string();
        let bare_version = self.version.split('_').next().unwrap_or(self.version);
        let major = bare_version.split('.').next().unwrap_or(bare_version);
        let major_minor = bare_version.splitn(3, '.').take(2).collect::<Vec<_>>().join(".");
        s.replace("{{prefix}}", &keg)
            .replace("{{opt_prefix}}", &self.ctx.opt().join(self.name).to_string_lossy())
            .replace("{{bin}}", &format!("{keg}/bin"))
            .replace("{{lib}}", &format!("{keg}/lib"))
            .replace("{{libexec}}", &format!("{keg}/libexec"))
            .replace("{{share}}", &format!("{keg}/share"))
            .replace("{{include}}", &format!("{keg}/include"))
            .replace("{{pkgshare}}", &format!("{keg}/share/{}", self.name))
            .replace("{{pkgetc}}", &format!("{prefix}/etc/{}", self.name))
            .replace("{{etc}}", &format!("{prefix}/etc"))
            .replace("{{var}}", &format!("{prefix}/var"))
            .replace("{{HOMEBREW_PREFIX}}", &prefix)
            .replace("{{HOMEBREW_CELLAR}}", &self.ctx.cellar().to_string_lossy())
            .replace("{{formula_name}}", self.name)
            .replace("{{version.major_minor}}", &major_minor)
            .replace("{{version.major}}", major)
            .replace("{{version}}", bare_version)
    }

    /// Resolve a {base, path} location object.
    fn loc(&self, v: &Value) -> Option<PathBuf> {
        let path = v.get("path").and_then(|p| p.as_str()).map(|p| self.template(p));
        let base = v.get("base").and_then(|b| b.as_str());
        let keg = self.keg();
        let prefix = &self.ctx.prefix;
        Some(match (base, path) {
            (None, Some(p)) => PathBuf::from(p),
            (Some(b), rel) => {
                let root = match b {
                    "prefix" => keg,
                    "opt_prefix" => self.ctx.opt().join(self.name),
                    "homebrew_prefix" => prefix.clone(),
                    "var" => prefix.join("var"),
                    "etc" => prefix.join("etc"),
                    // bin, lib, libexec, share, pkgshare... are keg-relative
                    "pkgshare" => keg.join("share").join(self.name),
                    "frameworks" => keg.join("Frameworks"),
                    other => keg.join(other),
                };
                match rel {
                    Some(r) => root.join(r),
                    None => root,
                }
            }
            (None, None) => return None,
        })
    }

    /// All guards must pass. `{a,b}` brace globs: any branch existing counts
    /// as existing.
    fn guards_pass(&self, step: &Value) -> bool {
        let Some(guards) = step.get("guards").and_then(|g| g.as_array()) else {
            return true;
        };
        guards.iter().all(|g| {
            match g.get("condition").and_then(|c| c.as_str()) {
                // Platform gate: {"condition": "on", "value": "macos"|"linux"}
                Some("on") => g.get("value").and_then(|v| v.as_str()) == Some("macos"),
                Some(cond @ ("if_exists" | "unless_exists")) => {
                    let Some(p) = self.loc(g) else { return true };
                    let exists = brace_expand(&p.to_string_lossy())
                        .iter()
                        .any(|c| Path::new(c).exists());
                    if cond == "if_exists" { exists } else { !exists }
                }
                _ => true,
            }
        })
    }
}

fn brace_expand(s: &str) -> Vec<String> {
    if let (Some(open), Some(close)) = (s.find('{'), s.find('}')) {
        if open < close {
            let (pre, rest) = s.split_at(open);
            let inner = &rest[1..close - open];
            let post = &rest[close - open + 1..];
            return inner
                .split(',')
                .flat_map(|alt| brace_expand(&format!("{pre}{alt}{post}")))
                .collect();
        }
    }
    vec![s.to_string()]
}

/// Run every step, returning human-readable notes (skips, failures, output
/// worth seeing). Never fails the install.
pub fn run_steps(ctx: &Ctx, name: &str, version: &str, steps: &[Value]) -> Vec<String> {
    let sc = StepCtx { ctx, name, version };
    let mut notes = Vec::new();
    for step in steps {
        if !sc.guards_pass(step) {
            continue;
        }
        let ty = step.get("type").and_then(|t| t.as_str()).unwrap_or("?");
        if let Err(e) = run_step(&sc, ty, step, &mut notes) {
            notes.push(format!("\x1b[33m!\x1b[0m {name}: post-install step '{ty}' failed: {e}"));
        }
    }
    notes
}

fn run_step(sc: &StepCtx, ty: &str, step: &Value, notes: &mut Vec<String>) -> anyhow::Result<()> {
    let name = sc.name;
    match ty {
        "mkdir_p" => {
            for p in paths(sc, step) {
                fs::create_dir_all(&p)?;
            }
        }
        "touch" => {
            for p in paths(sc, step) {
                if let Some(parent) = p.parent() {
                    fs::create_dir_all(parent)?;
                }
                if !p.exists() {
                    fs::File::create(&p)?;
                }
            }
        }
        "remove" => {
            for p in paths(sc, step) {
                if p.is_dir() {
                    let _ = fs::remove_dir_all(&p);
                } else {
                    let _ = fs::remove_file(&p);
                }
            }
        }
        "symlink" => {
            // ln semantics: an existing directory target means "link inside
            // it"; source_glob expands {brace} + * patterns to many sources.
            let (src, dst) = source_target(sc, step)?;
            let force = step.get("force").and_then(|f| f.as_bool()).unwrap_or(true);
            for s in expand_sources(&src, step) {
                let d = into_dir(&dst, &s);
                if let Some(parent) = d.parent() {
                    fs::create_dir_all(parent)?;
                }
                if force {
                    let _ = fs::remove_file(&d);
                }
                symlink(&s, &d)?;
            }
        }
        "link_dir" => {
            // Unlike symlink, target here is always the link's own name.
            let (src, dst) = source_target(sc, step)?;
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)?;
            }
            let _ = fs::remove_file(&dst);
            symlink(&src, &dst)?;
        }
        "link_children" => {
            let (src, dst) = source_target(sc, step)?;
            let suffix = step
                .get("suffix")
                .and_then(|s| s.as_str())
                .map(|s| sc.template(s))
                .unwrap_or_default();
            fs::create_dir_all(&dst)?;
            for entry in fs::read_dir(&src)? {
                let entry = entry?;
                let link = dst.join(format!("{}{suffix}", entry.file_name().to_string_lossy()));
                let _ = fs::remove_file(&link);
                symlink(entry.path(), link)?;
            }
        }
        "copy" => {
            // cp semantics: an existing directory target means "copy into it".
            let (src, dst) = source_target(sc, step)?;
            for s in expand_sources(&src, step) {
                let d = into_dir(&dst, &s);
                if let Some(parent) = d.parent() {
                    fs::create_dir_all(parent)?;
                }
                copy_recursive(&s, &d)?;
            }
        }
        "move" => {
            // mv semantics, same directory-target rule.
            let (src, dst) = source_target(sc, step)?;
            for s in expand_sources(&src, step) {
                let d = into_dir(&dst, &s);
                if let Some(parent) = d.parent() {
                    fs::create_dir_all(parent)?;
                }
                if step.get("overwrite").and_then(|o| o.as_bool()).unwrap_or(false) && d.exists() {
                    if d.is_dir() {
                        fs::remove_dir_all(&d)?;
                    } else {
                        fs::remove_file(&d)?;
                    }
                }
                fs::rename(&s, &d)?;
            }
        }
        "write" => {
            let p = sc.loc(step.get("path").unwrap_or(&Value::Null)).context_none()?;
            let content = sc.template(step.get("content").and_then(|c| c.as_str()).unwrap_or(""));
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent)?;
            }
            let overwrite = step.get("overwrite").and_then(|o| o.as_bool()).unwrap_or(false);
            if overwrite || !p.exists() {
                fs::write(&p, content)?;
            }
        }
        "set_permissions" => {
            let mode_str = step.get("permissions").and_then(|p| p.as_str()).unwrap_or("0755");
            let recursive = !step.get("non_recursive").and_then(|n| n.as_bool()).unwrap_or(false);
            for p in paths(sc, step) {
                apply_mode(&p, mode_str)?;
                if recursive && p.is_dir() {
                    for entry in walkdir::WalkDir::new(&p).into_iter().filter_map(|e| e.ok()) {
                        apply_mode(entry.path(), mode_str)?;
                    }
                }
            }
        }
        "inreplace" => {
            let p = sc.loc(step.get("path").unwrap_or(&Value::Null)).context_none()?;
            let before = sc.template(step.get("before").and_then(|b| b.as_str()).unwrap_or(""));
            let after = sc.template(step.get("after").and_then(|a| a.as_str()).unwrap_or(""));
            let content = fs::read_to_string(&p)?;
            let replaced = if step.get("regexp").and_then(|r| r.as_bool()).unwrap_or(false) {
                regex::Regex::new(&before)?.replace_all(&content, after.as_str()).into_owned()
            } else {
                content.replace(&before, &after)
            };
            if replaced != content {
                fs::write(&p, replaced)?;
            }
        }
        "install_gzipped_executable" => {
            let (src, dst) = source_target(sc, step)?;
            // Some platforms' bottles ship the executable already unpacked;
            // nothing to do as long as the target is there.
            if !src.exists() {
                if dst.exists() {
                    return Ok(());
                }
                anyhow::bail!("{} missing and {} not present", src.display(), dst.display());
            }
            let gz = fs::read(&src)?;
            let mut out = Vec::new();
            use std::io::Read;
            flate2::read::GzDecoder::new(&gz[..]).read_to_end(&mut out)?;
            fs::write(&dst, out)?;
            set_mode(&dst, 0o755)?;
        }
        "warn" => {
            if let Some(m) = step.get("message").and_then(|m| m.as_str()) {
                notes.push(format!("\x1b[33m!\x1b[0m {name}: {}", sc.template(m)));
            }
        }
        "run" => {
            let cmd = sc.loc(step.get("command").unwrap_or(&Value::Null)).context_none()?;
            let args: Vec<String> = step
                .get("args")
                .and_then(|a| a.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .map(|s| sc.template(s))
                        .collect()
                })
                .unwrap_or_default();
            let mut c = Command::new(&cmd);
            c.args(&args);
            if let Some(dir) = step.get("chdir").and_then(|d| sc.loc(d)) {
                c.current_dir(dir);
            }
            if let Some(env) = step.get("env").and_then(|e| e.as_object()) {
                for (k, v) in env {
                    if let Some(val) = v.as_str() {
                        c.env(k, sc.template(val));
                    }
                }
            }
            notes.push(format!(
                "· {name}: post-install ran {} {}",
                cmd.file_name().unwrap_or_default().to_string_lossy(),
                args.join(" ")
            ));
            let out = c.output()?;
            if !out.status.success() {
                let err = String::from_utf8_lossy(&out.stderr);
                anyhow::bail!("exit {}: {}", out.status, err.trim().chars().take(200).collect::<String>());
            }
        }
        "terminate_process" => {
            if let Some(pname) = step.get("name").and_then(|n| n.as_str()) {
                let _ = Command::new("pkill").arg("-f").arg(pname).output();
            }
        }
        "compile_gsettings_schemas" => tool_step(sc, step, notes, &[("glib", "glib-compile-schemas")], &[]),
        "gtk_update_icon_cache" => tool_step(
            sc,
            step,
            notes,
            &[
                ("gtk4", "gtk4-update-icon-cache"),
                ("gtk+3", "gtk3-update-icon-cache"),
                ("gtk+3", "gtk-update-icon-cache"),
            ],
            &["-q", "-t", "-f"],
        ),
        "gdk_pixbuf_query_loaders" => tool_step(
            sc,
            step,
            notes,
            &[("gdk-pixbuf", "gdk-pixbuf-query-loaders")],
            &["--update-cache"],
        ),
        "update_mime_database" => tool_step(sc, step, notes, &[("shared-mime-info", "update-mime-database")], &[]),
        "update_desktop_database" => {
            tool_step(sc, step, notes, &[("desktop-file-utils", "update-desktop-database")], &["-q"])
        }
        "init_data_dir" => {
            let Some(dir) = step.get("path").and_then(|p| sc.loc(p)) else { return Ok(()) };
            let empty = !dir.exists() || fs::read_dir(&dir).map(|mut d| d.next().is_none()).unwrap_or(true);
            fs::create_dir_all(&dir)?;
            if !empty {
                return Ok(());
            }
            let using = step.get("using").and_then(|u| u.as_str()).unwrap_or("");
            let tool = match using {
                "mariadb_install_db" => find_tool(sc.ctx, &[("mariadb", "mariadb-install-db")]),
                "mysql_install_db" => find_tool(sc.ctx, &[("mysql", "mysqld")]),
                "initdb" => find_tool(sc.ctx, &[("postgresql", "initdb")]),
                _ => None,
            };
            match (using, tool) {
                (_, Some(t)) => {
                    let arg = if using == "mysql_install_db" {
                        format!("--initialize-insecure --datadir={}", dir.display())
                    } else if using == "initdb" {
                        format!("--locale=C -E UTF-8 {}", dir.display())
                    } else {
                        format!("--datadir={}", dir.display())
                    };
                    notes.push(format!("· {name}: initializing data dir via {using}"));
                    let out = Command::new(&t).args(arg.split_whitespace()).output()?;
                    if !out.status.success() {
                        anyhow::bail!("{using} failed: {}", String::from_utf8_lossy(&out.stderr).trim().chars().take(200).collect::<String>());
                    }
                }
                (u, None) => notes.push(format!(
                    "\x1b[33m!\x1b[0m {name}: init_data_dir via '{u}' skipped (tool not installed)"
                )),
            }
        }
        "configure_gcc_runtime" | "configure_clang_system" | "configure_glibc_runtime"
        | "configure_php" | "bootstrap_cpython" | "bootstrap_pypy" => {
            notes.push(format!(
                "\x1b[33m!\x1b[0m {name}: post-install step '{ty}' not supported yet — some functionality may be missing"
            ));
        }
        other => {
            notes.push(format!(
                "\x1b[33m!\x1b[0m {name}: unknown post-install step '{other}' skipped"
            ));
        }
    }
    Ok(())
}

/// Steps that run a helper tool from another formula on a directory.
/// Missing tool or missing directory is a quiet skip — these maintain
/// GUI caches that don't exist unless the relevant stack is installed.
fn tool_step(
    sc: &StepCtx,
    step: &Value,
    notes: &mut Vec<String>,
    candidates: &[(&str, &str)],
    pre_args: &[&str],
) {
    let dir = step.get("path").and_then(|p| sc.loc(p));
    if let Some(d) = &dir {
        if !d.exists() {
            return;
        }
    }
    let Some(tool) = find_tool(sc.ctx, candidates) else { return };
    let mut c = Command::new(&tool);
    c.args(pre_args);
    if let Some(d) = &dir {
        c.arg(d);
    }
    match c.output() {
        Ok(out) if !out.status.success() => notes.push(format!(
            "\x1b[33m!\x1b[0m {}: {} failed: {}",
            sc.name,
            tool.file_name().unwrap_or_default().to_string_lossy(),
            String::from_utf8_lossy(&out.stderr).trim().chars().take(120).collect::<String>()
        )),
        Err(e) => notes.push(format!("\x1b[33m!\x1b[0m {}: {e}", sc.name)),
        _ => {}
    }
}

fn find_tool(ctx: &Ctx, candidates: &[(&str, &str)]) -> Option<PathBuf> {
    for (formula, bin) in candidates {
        let p = ctx.opt().join(formula).join("bin").join(bin);
        if p.exists() {
            return Some(p);
        }
        let p = ctx.bin().join(bin);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// `{brace}` + `*` expansion of a source path when the step sets
/// source_glob; otherwise the literal path.
fn expand_sources(src: &Path, step: &Value) -> Vec<PathBuf> {
    if !step.get("source_glob").and_then(|g| g.as_bool()).unwrap_or(false) {
        return vec![src.to_path_buf()];
    }
    let mut out = Vec::new();
    for pat in brace_expand(&src.to_string_lossy()) {
        if let Ok(matches) = glob::glob(&pat) {
            out.extend(matches.filter_map(|m| m.ok()));
        }
    }
    out
}

/// cp/ln target semantics: an existing directory means "inside it".
fn into_dir(dst: &Path, src: &Path) -> PathBuf {
    if dst.is_dir() {
        if let Some(name) = src.file_name() {
            return dst.join(name);
        }
    }
    dst.to_path_buf()
}

fn paths(sc: &StepCtx, step: &Value) -> Vec<PathBuf> {
    let raw: Vec<PathBuf> = if let Some(list) = step.get("paths").and_then(|p| p.as_array()) {
        list.iter().filter_map(|p| sc.loc(p)).collect()
    } else {
        step.get("path").and_then(|p| sc.loc(p)).into_iter().collect()
    };
    // Paths may carry glob patterns (python: venv/scripts/**/*).
    raw.into_iter()
        .flat_map(|p| {
            let s = p.to_string_lossy().into_owned();
            if s.contains('*') {
                brace_expand(&s)
                    .iter()
                    .flat_map(|pat| glob::glob(pat).ok().into_iter().flatten().filter_map(|m| m.ok()))
                    .collect()
            } else {
                vec![p]
            }
        })
        .collect()
}

/// chmod semantics: octal ("0755") or symbolic ("u+w", "a+rx", "go-w").
fn apply_mode(p: &Path, spec: &str) -> anyhow::Result<()> {
    let current = fs::metadata(p)?.permissions().mode();
    let mode = if let Ok(oct) = u32::from_str_radix(spec.trim_start_matches("0o"), 8) {
        (current & !0o7777) | oct
    } else {
        let op_at = spec
            .find(['+', '-'])
            .ok_or_else(|| anyhow::anyhow!("unsupported permission spec '{spec}'"))?;
        let (who, rest) = spec.split_at(op_at);
        let subtract = rest.starts_with('-');
        let mut bits = 0u32;
        for c in rest[1..].chars() {
            let per_class = match c {
                'r' => 0o4,
                'w' => 0o2,
                'x' => 0o1,
                _ => anyhow::bail!("unsupported permission char '{c}' in '{spec}'"),
            };
            let who = if who.is_empty() { "a" } else { who };
            for w in who.chars() {
                bits |= match w {
                    'u' => per_class << 6,
                    'g' => per_class << 3,
                    'o' => per_class,
                    'a' => per_class << 6 | per_class << 3 | per_class,
                    _ => anyhow::bail!("unsupported permission class '{w}' in '{spec}'"),
                };
            }
        }
        if subtract { current & !bits } else { current | bits }
    };
    set_mode(p, mode)?;
    Ok(())
}

fn source_target(sc: &StepCtx, step: &Value) -> anyhow::Result<(PathBuf, PathBuf)> {
    let src = sc.loc(step.get("source").unwrap_or(&Value::Null)).context_none()?;
    let dst = sc.loc(step.get("target").unwrap_or(&Value::Null)).context_none()?;
    Ok((src, dst))
}

fn copy_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    if src.is_dir() {
        fs::create_dir_all(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            copy_recursive(&entry.path(), &dst.join(entry.file_name()))?;
        }
    } else {
        fs::copy(src, dst)?;
    }
    Ok(())
}

fn set_mode(p: &Path, mode: u32) -> std::io::Result<()> {
    let mut perms = fs::metadata(p)?.permissions();
    perms.set_mode(mode);
    fs::set_permissions(p, perms)
}

/// Small helper so `Option<PathBuf>` slots into `?` chains with a message.
trait ContextNone<T> {
    fn context_none(self) -> anyhow::Result<T>;
}

impl<T> ContextNone<T> for Option<T> {
    fn context_none(self) -> anyhow::Result<T> {
        self.ok_or_else(|| anyhow::anyhow!("step is missing a required path"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brace_expansion() {
        assert_eq!(
            brace_expand("/m/{npm,npx}*"),
            vec!["/m/npm*".to_string(), "/m/npx*".to_string()]
        );
        assert_eq!(brace_expand("/plain/path"), vec!["/plain/path".to_string()]);
        assert_eq!(
            brace_expand("/{a,b}/{c,d}"),
            vec!["/a/c", "/a/d", "/b/c", "/b/d"]
        );
    }

    #[test]
    fn symbolic_and_octal_modes() {
        let p = std::env::temp_dir().join("dram-test-mode");
        fs::write(&p, "x").unwrap();
        apply_mode(&p, "0644").unwrap();
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o7777, 0o644);
        apply_mode(&p, "u+x").unwrap();
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o7777, 0o744);
        apply_mode(&p, "a+x").unwrap();
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o7777, 0o755);
        apply_mode(&p, "go-rx").unwrap();
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o7777, 0o700);
        assert!(apply_mode(&p, "banana").is_err());
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn into_dir_semantics() {
        // A non-existent target is used as-is.
        assert_eq!(
            into_dir(Path::new("/nonexistent/target"), Path::new("/src/npm")),
            PathBuf::from("/nonexistent/target")
        );
        // An existing directory target means "inside it".
        let tmp = std::env::temp_dir();
        assert_eq!(into_dir(&tmp, Path::new("/src/npm")), tmp.join("npm"));
    }
}
