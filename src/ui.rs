//! Terminal styling in one place, so the look stays consistent.

use indicatif::ProgressStyle;

pub const CHECK: &str = "\x1b[32m✓\x1b[0m";

pub fn dim(s: &str) -> String {
    format!("\x1b[2m{s}\x1b[0m")
}

/// Row label: bright name, dim version and tree branches, manually padded to
/// `width` visible columns. Padding is done here rather than in the template
/// because indicatif's `{msg:<N}` would count the ANSI codes.
pub fn row_label(branch: &str, name: &str, version: &str, width: usize) -> String {
    let visible = if branch.is_empty() {
        format!("{name} {version}")
    } else {
        format!("{branch} {name} {version}")
    }
    .chars()
    .count();

    let mut out = String::new();
    if !branch.is_empty() {
        out.push_str(&dim(branch));
        out.push(' ');
    }
    out.push_str(name);
    out.push(' ');
    out.push_str(&dim(version));
    out.push_str(&" ".repeat(width.saturating_sub(visible)));
    out
}

/// Active download row — percent is far easier to scan than KiB/KiB noise:
///   jq 1.8.1        ██████████████▎░░░░░░░░░░░░░░░  47%   4.2 MiB/s
pub fn download_style() -> ProgressStyle {
    ProgressStyle::with_template("  {msg} {bar:30.green} {percent:>3}%  {bytes_per_sec:>10}")
        .expect("static template")
        .progress_chars("█▉▊▋▌▍▎▏░")
}

/// The sticky bottom line: spinner + live activity summary. Everything else
/// flushes into scrollback above it.
pub fn sticky_style() -> ProgressStyle {
    ProgressStyle::with_template("{spinner:.green} {msg}")
        .expect("static template")
        .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", " "])
}

/// Label for an item row; deps are indented two columns inside the same
/// field width so the size/status columns stay aligned:
///   ripgrep 15.2.0     2.02 MiB ✓
///     libgit2 1.9.6    1.86 MiB ✓ (bat)
pub fn item_label(name: &str, version: &str, width: usize, dep: bool) -> String {
    if dep {
        format!(
            "  {}",
            row_label("", name, version, width.saturating_sub(2))
        )
    } else {
        row_label("", name, version, width)
    }
}

/// Permanent line flushed into scrollback when a formula is fully installed.
pub fn flushed_row(
    name: &str,
    version: &str,
    width: usize,
    size: u64,
    via: Option<&str>,
) -> String {
    format!(
        "  {} {:>10} {}{}",
        item_label(name, version, width, via.is_some()),
        indicatif::HumanBytes(size).to_string(),
        CHECK,
        via.map(|v| dim(&format!(" ({v})"))).unwrap_or_default()
    )
}
