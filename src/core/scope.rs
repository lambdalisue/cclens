//! Scope routing: which config layer owns each finding. "Optimize my global
//! setup" and "optimize this project" are different tasks, so every report
//! routes each finding to the layer whose config carries the fix — a surface
//! wedge by the surface's own scope, friction by where it concentrates
//! (`docs/specs/cli.md`).

/// Which scope a read command reports on (`--scope`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeFilter {
    /// Both layers, split into sections (the default).
    All,
    Global,
    /// Project-scoped findings — all projects, or one when a slug is given.
    Project(Option<String>),
}

impl ScopeFilter {
    /// Parse a `--scope` value: `global` | `project` | `project:<slug>`.
    pub fn parse(value: &str) -> Option<ScopeFilter> {
        match value {
            "global" => Some(ScopeFilter::Global),
            "project" => Some(ScopeFilter::Project(None)),
            other => other
                .strip_prefix("project:")
                .filter(|slug| !slug.is_empty())
                .map(|slug| Some(ScopeFilter::Project(Some(slug.to_string()))))?,
        }
    }

    pub fn includes_global(&self) -> bool {
        matches!(self, ScopeFilter::All | ScopeFilter::Global)
    }

    pub fn includes_project(&self, project: &str) -> bool {
        match self {
            ScopeFilter::All | ScopeFilter::Project(None) => true,
            ScopeFilter::Global => false,
            ScopeFilter::Project(Some(slug)) => slug == project,
        }
    }
}

/// A friction category routed to the global section: no single project owns a
/// strict majority of it, so it reads as a cross-project habit — the fix
/// belongs in global config, not any one project's.
#[derive(Debug, PartialEq)]
pub struct GlobalFriction {
    pub category: String,
    pub total: i64,
    /// How many projects the failures spread across.
    pub projects: usize,
}

/// Friction routed by ownership: majority-owned categories under their project,
/// the spread remainder under global.
#[derive(Debug, PartialEq, Default)]
pub struct FrictionSplit {
    pub global: Vec<GlobalFriction>,
    /// `(project, [(category, count in that project)])`, busiest project first.
    pub per_project: Vec<(String, Vec<(String, i64)>)>,
}

/// Route `(project, category, count)` friction cells to the config layer that
/// owns the fix. A category whose count concentrates in one project (strict
/// majority, the same rule the summary's "mostly in" line used) is that
/// project's finding — fix it in that project's config. A category with no
/// majority owner is a global finding, reported with its spread.
pub fn split_friction(cells: &[(String, String, i64)]) -> FrictionSplit {
    use std::collections::BTreeMap;

    // category -> per-project cells
    let mut by_category: BTreeMap<&str, Vec<(&str, i64)>> = BTreeMap::new();
    for (project, category, count) in cells {
        by_category
            .entry(category)
            .or_default()
            .push((project, *count));
    }

    let mut global = Vec::new();
    let mut per_project: BTreeMap<&str, Vec<(String, i64)>> = BTreeMap::new();
    for (category, cells) in by_category {
        let total: i64 = cells.iter().map(|(_, n)| n).sum();
        let owner = cells
            .iter()
            .max_by_key(|(_, n)| *n)
            .filter(|(_, n)| n * 2 > total);
        match owner {
            Some((project, count)) => per_project
                .entry(project)
                .or_default()
                .push((category.to_string(), *count)),
            None => global.push(GlobalFriction {
                category: category.to_string(),
                total,
                projects: cells.len(),
            }),
        }
    }

    global.sort_by(|a, b| {
        b.total
            .cmp(&a.total)
            .then_with(|| a.category.cmp(&b.category))
    });
    let mut per_project: Vec<(String, Vec<(String, i64)>)> = per_project
        .into_iter()
        .map(|(project, mut categories)| {
            categories.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            (project.to_string(), categories)
        })
        .collect();
    per_project.sort_by(|a, b| {
        let total = |cats: &[(String, i64)]| cats.iter().map(|(_, n)| n).sum::<i64>();
        total(&b.1).cmp(&total(&a.1)).then_with(|| a.0.cmp(&b.0))
    });
    FrictionSplit {
        global,
        per_project,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(project: &str, category: &str, count: i64) -> (String, String, i64) {
        (project.to_string(), category.to_string(), count)
    }

    #[test]
    fn parse_accepts_global_project_and_slugged_project() {
        assert_eq!(ScopeFilter::parse("global"), Some(ScopeFilter::Global));
        assert_eq!(
            ScopeFilter::parse("project"),
            Some(ScopeFilter::Project(None))
        );
        assert_eq!(
            ScopeFilter::parse("project:alpha"),
            Some(ScopeFilter::Project(Some("alpha".to_string())))
        );
        assert_eq!(ScopeFilter::parse("project:"), None);
        assert_eq!(ScopeFilter::parse("all"), None);
    }

    #[test]
    fn filters_match_their_scopes() {
        assert!(ScopeFilter::All.includes_global());
        assert!(ScopeFilter::All.includes_project("alpha"));
        assert!(ScopeFilter::Global.includes_global());
        assert!(!ScopeFilter::Global.includes_project("alpha"));
        let one = ScopeFilter::Project(Some("alpha".to_string()));
        assert!(!one.includes_global());
        assert!(one.includes_project("alpha"));
        assert!(!one.includes_project("beta"));
    }

    #[test]
    fn a_majority_category_routes_to_its_project() {
        // alpha owns 2 of 3 edit-precondition failures — a strict majority.
        let split = split_friction(&[
            cell("alpha", "edit-precondition", 2),
            cell("beta", "edit-precondition", 1),
        ]);
        assert!(split.global.is_empty());
        assert_eq!(
            split.per_project,
            vec![(
                "alpha".to_string(),
                vec![("edit-precondition".to_string(), 2)]
            )]
        );
    }

    #[test]
    fn a_spread_category_routes_to_global() {
        // 2/2/2 across three projects: no owner — a cross-project habit.
        let split = split_friction(&[
            cell("alpha", "blocked-by-hook", 2),
            cell("beta", "blocked-by-hook", 2),
            cell("gamma", "blocked-by-hook", 2),
        ]);
        assert!(split.per_project.is_empty());
        assert_eq!(
            split.global,
            vec![GlobalFriction {
                category: "blocked-by-hook".to_string(),
                total: 6,
                projects: 3,
            }]
        );
    }

    #[test]
    fn projects_rank_by_their_owned_friction_volume() {
        let split = split_friction(&[
            cell("alpha", "edit-precondition", 3),
            cell("beta", "path-not-found", 10),
        ]);
        // beta owns more friction, so it leads.
        let projects: Vec<&str> = split.per_project.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(projects, vec!["beta", "alpha"]);
    }
}
