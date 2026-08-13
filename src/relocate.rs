//! Bottle relocation — the part of the blueprint that actually bites.
//!
//! Bottles ship with `@@HOMEBREW_PREFIX@@` / `@@HOMEBREW_CELLAR@@`
//! placeholders: in text files as literal strings, and inside Mach-O load
//! commands (LC_ID_DYLIB, LC_LOAD_DYLIB, LC_RPATH) where `brew bottle`
//! wrote them with install_name_tool. We do the reverse of `brew` pouring:
//! rewrite placeholders to our real prefix.
//!
//! v1 shells out to Xcode's otool / install_name_tool / codesign rather
//! than parsing Mach-O ourselves (goblin can replace this later). Crucially,
//! any install_name_tool edit invalidates the code signature, and on Apple
//! silicon an invalid signature means SIGKILL on exec — so every patched
//! binary gets re-ad-hoc-signed.

use crate::Ctx;
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use walkdir::WalkDir;

const PLACEHOLDER_PREFIX: &str = "@@HOMEBREW_PREFIX@@";
const PLACEHOLDER_CELLAR: &str = "@@HOMEBREW_CELLAR@@";
const PLACEHOLDER_LIBRARY: &str = "@@HOMEBREW_LIBRARY@@";

/// Rewrite placeholders in one string. `depmap` (formula name -> keg version)
/// makes relocation *version-pinned*: `@@HOMEBREW_PREFIX@@/opt/<dep>/...`
/// becomes `<cellar>/<dep>/<version>/...` rather than going through the
/// mutable opt/ symlink. That turns kegs into immutable artifacts that keep
/// working no matter what gets installed or upgraded later — the property
/// per-project environments are built on. Deps not in the map (shouldn't
/// happen — the map covers the whole resolved closure) fall back to opt/.
pub fn substitute(s: &str, prefix: &str, cellar: &str, depmap: &HashMap<String, String>) -> String {
    let s = s.replace(PLACEHOLDER_CELLAR, cellar);
    let s = s.replace(PLACEHOLDER_LIBRARY, &format!("{prefix}/Library"));

    const OPT: &str = "@@HOMEBREW_PREFIX@@/opt/";
    let mut out = String::with_capacity(s.len());
    let mut rest = s.as_str();
    while let Some(i) = rest.find(OPT) {
        out.push_str(&rest[..i]);
        let after = &rest[i + OPT.len()..];
        let name_end = after.find('/').unwrap_or(after.len());
        let name = &after[..name_end];
        match depmap.get(name) {
            Some(version) => out.push_str(&format!("{cellar}/{name}/{version}")),
            None => out.push_str(&format!("{prefix}/opt/{name}")),
        }
        rest = &after[name_end..];
    }
    out.push_str(rest);
    out.replace(PLACEHOLDER_PREFIX, prefix)
}

pub fn relocate_keg(ctx: &Ctx, keg: &Path, depmap: &HashMap<String, String>) -> Result<()> {
    let prefix = ctx.prefix.to_string_lossy().to_string();
    let cellar = ctx.cellar().to_string_lossy().to_string();

    for entry in WalkDir::new(keg).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let Ok(head) = read_head(path) else { continue };
        if is_macho(&head) {
            relocate_macho(path, &prefix, &cellar, depmap)?;
        } else {
            relocate_text(path, &prefix, &cellar, depmap)?;
        }
    }
    Ok(())
}

fn read_head(path: &Path) -> std::io::Result<[u8; 4]> {
    use std::io::Read;
    let mut f = fs::File::open(path)?;
    let mut buf = [0u8; 4];
    let n = f.read(&mut buf)?;
    if n < 4 {
        buf = [0; 4];
    }
    Ok(buf)
}

pub(crate) fn is_macho_path(path: &Path) -> bool {
    read_head(path).map(|h| is_macho(&h)).unwrap_or(false)
}

fn is_macho(head: &[u8; 4]) -> bool {
    matches!(
        u32::from_be_bytes(*head),
        0xfeedface | 0xfeedfacf | 0xcefaedfe | 0xcffaedfe | 0xcafebabe | 0xcafebabf
    )
}

/// Rewrite placeholder paths in one Mach-O file's load commands, then re-sign.
fn relocate_macho(
    path: &Path,
    prefix: &str,
    cellar: &str,
    depmap: &HashMap<String, String>,
) -> Result<()> {
    // Java .class files share the 0xcafebabe magic with fat Mach-O; if otool
    // rejects the file it wasn't a real Mach-O, so leave it alone.
    let Ok(text) = run("otool", &["-l".as_ref(), path.as_os_str()]) else {
        return Ok(());
    };

    // Collect every placeholder-bearing path out of LC_ID_DYLIB /
    // LC_LOAD_DYLIB / LC_RPATH. otool -l prints them as
    //   "name @@HOMEBREW_PREFIX@@/... (offset 24)" / "path ... (offset 12)".
    let mut id: Option<String> = None;
    let mut loads: Vec<String> = Vec::new();
    let mut rpaths: Vec<String> = Vec::new();
    let mut current_cmd = "";
    for line in text.lines() {
        let line = line.trim();
        if let Some(cmd) = line.strip_prefix("cmd ") {
            current_cmd = match cmd {
                "LC_ID_DYLIB" => "id",
                "LC_LOAD_DYLIB" | "LC_LOAD_WEAK_DYLIB" | "LC_REEXPORT_DYLIB" => "load",
                "LC_RPATH" => "rpath",
                _ => "",
            };
            continue;
        }
        let value = line
            .strip_prefix("name ")
            .or_else(|| line.strip_prefix("path "))
            .and_then(|v| v.rsplit_once(" (offset").map(|(p, _)| p.to_string()));
        if let Some(v) = value {
            if !v.contains("@@HOMEBREW") {
                continue;
            }
            match current_cmd {
                "id" => id = Some(v),
                "load" => loads.push(v),
                "rpath" => rpaths.push(v),
                _ => {}
            }
        }
    }

    if id.is_none() && loads.is_empty() && rpaths.is_empty() {
        return Ok(());
    }

    // install_name_tool needs write permission; bottles often ship r-xr-xr-x.
    let perms = fs::metadata(path)?.permissions();
    let mut writable = perms.clone();
    writable.set_mode(perms.mode() | 0o200);
    fs::set_permissions(path, writable)?;

    let subst = |s: &str| substitute(s, prefix, cellar, depmap);

    let mut args: Vec<String> = Vec::new();
    if let Some(old) = &id {
        args.extend(["-id".into(), subst(old)]);
    }
    for old in &loads {
        args.extend(["-change".into(), old.clone(), subst(old)]);
    }
    for old in &rpaths {
        args.extend(["-rpath".into(), old.clone(), subst(old)]);
    }
    args.push(path.to_string_lossy().into_owned());
    let arg_refs: Vec<&std::ffi::OsStr> = args.iter().map(|s| s.as_ref()).collect();
    run("install_name_tool", &arg_refs)?;

    // The edit above invalidated the signature; re-ad-hoc-sign or the
    // binary is killed on exec on Apple silicon.
    run(
        "codesign",
        &[
            "--force".as_ref(),
            "--sign".as_ref(),
            "-".as_ref(),
            "--preserve-metadata=entitlements,requirements,flags".as_ref(),
            path.as_os_str(),
        ],
    )?;

    fs::set_permissions(path, perms)?;
    Ok(())
}

/// Replace placeholders in text files (pkg-config .pc files, shell scripts,
/// shebangs, cmake configs...). Binary-safe: we only touch files that look
/// like UTF-8 and contain a placeholder.
fn relocate_text(
    path: &Path,
    prefix: &str,
    cellar: &str,
    depmap: &HashMap<String, String>,
) -> Result<()> {
    const MARKER: &[u8] = b"@@HOMEBREW";
    let Ok(bytes) = fs::read(path) else { return Ok(()) };
    if bytes.len() < MARKER.len() || !bytes.windows(MARKER.len()).any(|w| w == MARKER) {
        return Ok(());
    }
    let Ok(content) = String::from_utf8(bytes) else { return Ok(()) };
    let replaced = substitute(&content, prefix, cellar, depmap);
    if replaced != content {
        let perms = fs::metadata(path)?.permissions();
        let mut writable = perms.clone();
        writable.set_mode(perms.mode() | 0o200);
        fs::set_permissions(path, writable)?;
        fs::write(path, replaced)?;
        fs::set_permissions(path, perms)?;
    }
    Ok(())
}

fn run(cmd: &str, args: &[&std::ffi::OsStr]) -> Result<String> {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .with_context(|| format!("running {cmd} (is Xcode / CLT installed?)"))?;
    if !out.status.success() {
        bail!(
            "{cmd} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn depmap() -> HashMap<String, String> {
        HashMap::from([("oniguruma".to_string(), "6.9.10".to_string())])
    }

    #[test]
    fn opt_reference_pins_to_exact_version() {
        assert_eq!(
            substitute(
                "@@HOMEBREW_PREFIX@@/opt/oniguruma/lib/libonig.5.dylib",
                "/p",
                "/p/Cellar",
                &depmap()
            ),
            "/p/Cellar/oniguruma/6.9.10/lib/libonig.5.dylib"
        );
    }

    #[test]
    fn unknown_dep_falls_back_to_opt() {
        assert_eq!(
            substitute("@@HOMEBREW_PREFIX@@/opt/zzz/lib/x.dylib", "/p", "/p/Cellar", &depmap()),
            "/p/opt/zzz/lib/x.dylib"
        );
    }

    #[test]
    fn plain_prefix_and_cellar() {
        assert_eq!(
            substitute("@@HOMEBREW_PREFIX@@/share/doc", "/p", "/p/Cellar", &depmap()),
            "/p/share/doc"
        );
        assert_eq!(
            substitute("@@HOMEBREW_CELLAR@@/foo/1.0/lib", "/p", "/p/Cellar", &depmap()),
            "/p/Cellar/foo/1.0/lib"
        );
        assert_eq!(
            substitute("@@HOMEBREW_LIBRARY@@/Homebrew", "/p", "/p/Cellar", &depmap()),
            "/p/Library/Homebrew"
        );
    }

    #[test]
    fn multiple_opt_references_in_one_string() {
        let s = "-L@@HOMEBREW_PREFIX@@/opt/oniguruma/lib -I@@HOMEBREW_PREFIX@@/opt/zzz/include";
        assert_eq!(
            substitute(s, "/p", "/p/Cellar", &depmap()),
            "-L/p/Cellar/oniguruma/6.9.10/lib -I/p/opt/zzz/include"
        );
    }
}
