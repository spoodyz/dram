//! Dramfile support — chain-install a whole list of formulae from a file,
//! and dump the currently-requested set back out. Brew needs the external
//! `brew bundle` extension for this; here it's first-class.
//!
//! Format: one formula name per line; blank lines and `#` comments ignored.

use crate::api;
use crate::install;
use crate::resolver;
use crate::ui;
use crate::Ctx;
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::Path;

pub const DEFAULT_FILE: &str = "Dramfile";

pub fn parse(path: &Path) -> Result<Vec<String>> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("no {} here — create one or pass a path", path.display()))?;
    let names: Vec<String> = raw
        .lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();
    if names.is_empty() {
        bail!("{} lists no formulae", path.display());
    }
    Ok(names)
}

/// `dram bundle [file]` — install everything the file lists, as one plan.
pub async fn install(ctx: &Ctx, path: &Path) -> Result<()> {
    let names = parse(path)?;
    println!(
        "{}",
        ui::dim(&format!("bundle: {} ({} formulae)", path.display(), names.len()))
    );
    let index = api::fetch_index(ctx, false).await?;
    let plan = resolver::resolve(&index, &names)?;
    let roots: Vec<String> = names
        .iter()
        .filter_map(|n| index.get(n).map(|f| f.name.clone()))
        .collect();
    install::install_all(ctx, &plan, &roots).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_names_comments_and_blanks() {
        let p = std::env::temp_dir().join("dram-test-Dramfile");
        fs::write(&p, "# header comment\n\njq  # inline comment\nwget\n   \n").unwrap();
        assert_eq!(parse(&p).unwrap(), vec!["jq".to_string(), "wget".to_string()]);
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn empty_file_is_an_error() {
        let p = std::env::temp_dir().join("dram-test-Dramfile-empty");
        fs::write(&p, "# only comments\n\n").unwrap();
        assert!(parse(&p).is_err());
        let _ = fs::remove_file(&p);
    }
}

/// `dram bundle --dump [file]` — write the explicitly-requested formulae
/// (not deps; they'll be re-resolved on install) to the file.
pub fn dump(ctx: &Ctx, path: &Path) -> Result<()> {
    let mut names = install::requested_names(ctx)?;
    names.sort();
    if names.is_empty() {
        bail!("nothing explicitly installed to dump");
    }
    let mut out = String::from("# Dramfile — install with `dram bundle`\n");
    for n in &names {
        out.push_str(n);
        out.push('\n');
    }
    fs::write(path, out)?;
    println!(
        "{} wrote {} formula{} to {}",
        ui::CHECK,
        names.len(),
        if names.len() == 1 { "" } else { "e" },
        path.display()
    );
    Ok(())
}
