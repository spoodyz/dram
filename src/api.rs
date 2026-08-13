use crate::formula::Formula;
use crate::Ctx;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::time::{Duration, SystemTime};

const INDEX_URL: &str = "https://formulae.brew.sh/api/formula.json";
const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// The formula index: name -> formula, with aliases and old names
/// folded in so `dram install python` finds python@3.x.
pub struct Index {
    formulae: Vec<Formula>,
    by_name: HashMap<String, usize>,
}

impl Index {
    pub fn from_formulae(formulae: Vec<Formula>) -> Index {
        let mut by_name = HashMap::with_capacity(formulae.len() * 2);
        for (i, f) in formulae.iter().enumerate() {
            by_name.insert(f.name.clone(), i);
            for alias in f.aliases.iter().chain(f.oldnames.iter()) {
                by_name.entry(alias.clone()).or_insert(i);
            }
        }
        Index { formulae, by_name }
    }

    pub fn get(&self, name: &str) -> Option<&Formula> {
        self.by_name.get(name).map(|&i| &self.formulae[i])
    }

    pub fn all(&self) -> impl Iterator<Item = &Formula> {
        self.formulae.iter()
    }
}

pub async fn fetch_index(ctx: &Ctx, force: bool) -> Result<Index> {
    let cache_path = ctx.cache().join("formula.json");

    let fresh = !force
        && cache_path
            .metadata()
            .and_then(|m| m.modified())
            .map(|t| SystemTime::now().duration_since(t).unwrap_or(CACHE_TTL) < CACHE_TTL)
            .unwrap_or(false);

    let raw = if fresh {
        tokio::fs::read(&cache_path).await?
    } else {
        eprintln!("{}", crate::ui::dim("· fetching formula index"));
        let body = reqwest::Client::new()
            .get(INDEX_URL)
            .send()
            .await
            .context("fetching formula.json")?
            .error_for_status()?
            .bytes()
            .await?;
        tokio::fs::write(&cache_path, &body).await?;
        body.to_vec()
    };

    let formulae: Vec<Formula> = serde_json::from_slice(&raw).context("parsing formula.json")?;
    Ok(Index::from_formulae(formulae))
}
