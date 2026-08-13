use serde::Deserialize;
use std::collections::HashMap;

/// One entry from https://formulae.brew.sh/api/formula.json.
/// Only the fields dram needs — serde ignores the rest of the ~25MB dump.
#[derive(Debug, Clone, Deserialize)]
pub struct Formula {
    pub name: String,
    pub desc: Option<String>,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub oldnames: Vec<String>,
    pub versions: Versions,
    /// Bump appended to the keg dir name as `_N` when > 0.
    #[serde(default)]
    pub revision: u32,
    /// Runtime dependencies only — build deps never matter for bottles.
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub keg_only: bool,
    #[serde(default)]
    pub bottle: HashMap<String, BottleSpec>,
    /// Declarative post-install steps (typed JSON, interpreted natively).
    #[serde(default)]
    pub post_install_steps: Vec<serde_json::Value>,
}

impl Formula {
    /// Directory name under Cellar/<name>/, e.g. "1.7.1" or "1.7.1_2".
    pub fn keg_version(&self) -> String {
        let stable = self.versions.stable.as_deref().unwrap_or("unknown");
        if self.revision > 0 {
            format!("{stable}_{}", self.revision)
        } else {
            stable.to_string()
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Versions {
    pub stable: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BottleSpec {
    /// Keyed by platform tag: "arm64_sequoia", "sonoma", "all", ...
    #[serde(default)]
    pub files: HashMap<String, BottleFile>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BottleFile {
    /// ":any" / ":any_skip_relocation" mean relocatable; an absolute
    /// path means the cellar location is baked into the binaries.
    #[serde(default)]
    pub cellar: String,
    pub url: String,
    pub sha256: String,
}
