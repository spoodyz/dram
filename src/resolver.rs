use crate::api::Index;
use crate::formula::Formula;
use anyhow::{bail, Context, Result};
use std::collections::HashSet;

/// Resolve the runtime dependency closure of `roots` into install order
/// (dependencies before dependents) via post-order DFS.
pub fn resolve<'a>(index: &'a Index, roots: &[String]) -> Result<Vec<&'a Formula>> {
    let mut order: Vec<&Formula> = Vec::new();
    let mut done: HashSet<&str> = HashSet::new();
    let mut in_progress: HashSet<&str> = HashSet::new();

    for root in roots {
        let f = index
            .get(root)
            .with_context(|| format!("no such formula: {root}"))?;
        visit(index, f, &mut done, &mut in_progress, &mut order)?;
    }
    Ok(order)
}

fn visit<'a>(
    index: &'a Index,
    f: &'a Formula,
    done: &mut HashSet<&'a str>,
    in_progress: &mut HashSet<&'a str>,
    order: &mut Vec<&'a Formula>,
) -> Result<()> {
    if done.contains(f.name.as_str()) {
        return Ok(());
    }
    if !in_progress.insert(&f.name) {
        // Homebrew/core has no runtime cycles, but don't hang if one appears.
        bail!("dependency cycle involving {}", f.name);
    }
    for dep in &f.dependencies {
        let d = index
            .get(dep)
            .with_context(|| format!("{} depends on unknown formula {dep}", f.name))?;
        visit(index, d, done, in_progress, order)?;
    }
    in_progress.remove(f.name.as_str());
    done.insert(&f.name);
    order.push(f);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Index;
    use crate::formula::{Formula, Versions};
    use std::collections::HashMap;

    fn f(name: &str, deps: &[&str]) -> Formula {
        Formula {
            name: name.into(),
            desc: None,
            aliases: vec![],
            oldnames: vec![],
            versions: Versions { stable: Some("1.0".into()) },
            revision: 0,
            dependencies: deps.iter().map(|s| s.to_string()).collect(),
            keg_only: false,
            bottle: HashMap::new(),
            post_install_steps: vec![],
        }
    }

    fn names(plan: &[&Formula]) -> Vec<String> {
        plan.iter().map(|f| f.name.clone()).collect()
    }

    #[test]
    fn deps_before_dependents() {
        let index = Index::from_formulae(vec![f("a", &["b"]), f("b", &["c"]), f("c", &[])]);
        let plan = resolve(&index, &["a".into()]).unwrap();
        assert_eq!(names(&plan), ["c", "b", "a"]);
    }

    #[test]
    fn shared_dep_appears_once() {
        let index = Index::from_formulae(vec![f("a", &["c"]), f("b", &["c"]), f("c", &[])]);
        let plan = resolve(&index, &["a".into(), "b".into()]).unwrap();
        assert_eq!(names(&plan), ["c", "a", "b"]);
    }

    #[test]
    fn cycle_is_an_error_not_a_hang() {
        let index = Index::from_formulae(vec![f("a", &["b"]), f("b", &["a"])]);
        assert!(resolve(&index, &["a".into()]).is_err());
    }

    #[test]
    fn unknown_formula_is_an_error() {
        let index = Index::from_formulae(vec![f("a", &[])]);
        assert!(resolve(&index, &["nope".into()]).is_err());
    }
}
