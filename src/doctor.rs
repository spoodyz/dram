//! `dram doctor` — verify the Cellar is actually healthy: no dangling
//! links, every prefix-local dylib reference resolves, and signatures are
//! valid (an invalid signature means SIGKILL on exec on Apple silicon).

use crate::relocate;
use crate::ui;
use crate::Ctx;
use anyhow::Result;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn doctor(ctx: &Ctx) -> Result<()> {
    let mut problems: Vec<String> = Vec::new();

    // Dangling symlinks in the shared namespaces.
    for dir in [ctx.bin(), ctx.opt()] {
        if !dir.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let p = entry.path();
            if fs::read_link(&p).is_ok() && !p.exists() {
                problems.push(format!("dangling symlink: {}", p.display()));
            }
        }
    }

    // Per keg: executables and top-level dylibs must have resolvable
    // prefix-local load commands and valid signatures.
    let prefix = ctx.prefix.to_string_lossy().to_string();
    let mut kegs = 0usize;
    let mut checked = 0usize;
    for name_dir in fs::read_dir(ctx.cellar())?.filter_map(|e| e.ok()) {
        if !name_dir.path().is_dir() {
            continue;
        }
        for keg in fs::read_dir(name_dir.path())?.filter_map(|e| e.ok()) {
            if !keg.path().is_dir() {
                continue;
            }
            kegs += 1;
            for sub in ["bin", "lib"] {
                let dir = keg.path().join(sub);
                if !dir.is_dir() {
                    continue;
                }
                for entry in fs::read_dir(&dir)?.filter_map(|e| e.ok()) {
                    let p = entry.path();
                    // Only real files (not symlinks) that are Mach-O.
                    if !p.is_file() || fs::read_link(&p).is_ok() || !relocate::is_macho_path(&p) {
                        continue;
                    }
                    checked += 1;
                    check_macho(&p, &prefix, &mut problems);
                }
            }
        }
    }

    if problems.is_empty() {
        println!(
            "{} no problems found ({kegs} kegs, {checked} binaries checked)",
            ui::CHECK
        );
    } else {
        for p in &problems {
            println!("\x1b[33m!\x1b[0m {p}");
        }
        println!(
            "{} problem{} found",
            problems.len(),
            if problems.len() == 1 { "" } else { "s" }
        );
    }
    Ok(())
}

fn check_macho(p: &Path, prefix: &str, problems: &mut Vec<String>) {
    // Every prefix-local load command must point at something that exists.
    if let Ok(out) = Command::new("otool").arg("-L").arg(p).output() {
        for line in String::from_utf8_lossy(&out.stdout).lines().skip(1) {
            let dep = line.trim().split(" (").next().unwrap_or("").trim();
            if dep.starts_with(prefix) && !PathBuf::from(dep).exists() {
                problems.push(format!("{}: missing dylib {dep}", p.display()));
            }
            if dep.contains("@@HOMEBREW") {
                problems.push(format!("{}: unrelocated placeholder {dep}", p.display()));
            }
        }
    }
    // Invalid signature = SIGKILL on exec.
    if let Ok(status) = Command::new("codesign").arg("--verify").arg(p).output() {
        if !status.status.success() {
            problems.push(format!("{}: invalid code signature", p.display()));
        }
    }
}
