//! Parse Claude Code config into the surface catalog. Like the transcript
//! adapter, the parsing core is pure (over file content) and the directory
//! walking is a thin shell. See `docs/specs/config-format.md`.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use crate::core::surface::{LoadMode, Scope, Surface};

/// Approximate the token weight of injected text. A ranking signal, not a
/// billing figure (`docs/specs/config-format.md`), so a cheap, consistent
/// estimate is enough: roughly four characters per token.
pub fn approx_tokens(text: &str) -> u64 {
    (text.chars().count() as u64).div_ceil(4)
}

/// Build a `skill` surface from one `SKILL.md`. Skills load only their
/// description at startup, so the load mode is `StartupDescription`: the static
/// cost is the whole file (what is paid on-demand when invoked) and the startup
/// cost is the frontmatter `description` alone.
pub fn skill_surface(id: &str, config_path: &str, content: &str, scope: &Scope) -> Surface {
    Surface {
        kind: "skill".to_string(),
        id: id.to_string(),
        scope: scope.clone(),
        config_path: config_path.to_string(),
        static_tokens: Some(approx_tokens(content)),
        startup_tokens: Some(description_tokens(content)),
        load_mode: LoadMode::StartupDescription,
    }
}

/// The startup weight of a description-loaded surface: its frontmatter
/// `description`, or zero when it declares none. Only the description text is
/// counted — the listing scaffolding Claude Code wraps it in is not ours to
/// weigh (`docs/specs/config-format.md`).
fn description_tokens(content: &str) -> u64 {
    frontmatter_description(content).map_or(0, |d| approx_tokens(&d))
}

/// Read every `<name>/SKILL.md` under a skills directory into surfaces. A
/// missing directory yields nothing (the scope may simply not exist).
pub fn read_skill_surfaces(skills_dir: &Path, scope: &Scope) -> Vec<Surface> {
    let Ok(entries) = fs::read_dir(skills_dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let skill_md = entry.path().join("SKILL.md");
            let id = entry.file_name().to_str()?.to_string();
            let content = fs::read_to_string(&skill_md).ok()?;
            Some(skill_surface(
                &id,
                &skill_md.display().to_string(),
                &content,
                scope,
            ))
        })
        .collect()
}

/// Build a `rule` surface. A rule with a `paths:` frontmatter key is loaded
/// only when a matching file is in play (`PathConditional`); one without is
/// always loaded (`StartupFull`). See `docs/specs/config-format.md`.
pub fn rule_surface(id: &str, config_path: &str, content: &str, scope: &Scope) -> Surface {
    let tokens = approx_tokens(content);
    let (load_mode, startup_tokens) = if has_paths_frontmatter(content) {
        // Nothing until a matching file is in play; then the whole body.
        (LoadMode::PathConditional, 0)
    } else {
        (LoadMode::StartupFull, tokens)
    };
    Surface {
        kind: "rule".to_string(),
        id: id.to_string(),
        scope: scope.clone(),
        config_path: config_path.to_string(),
        static_tokens: Some(tokens),
        startup_tokens: Some(startup_tokens),
        load_mode,
    }
}

/// Build an `agent` surface. Like skills, only the description loads at startup.
pub fn agent_surface(id: &str, config_path: &str, content: &str, scope: &Scope) -> Surface {
    Surface {
        kind: "agent".to_string(),
        id: id.to_string(),
        scope: scope.clone(),
        config_path: config_path.to_string(),
        static_tokens: Some(approx_tokens(content)),
        startup_tokens: Some(description_tokens(content)),
        load_mode: LoadMode::StartupDescription,
    }
}

/// Build a `claude_md` surface — always-on context paid every session, so its
/// whole text is startup cost.
pub fn claude_md_surface(id: &str, config_path: &str, content: &str, scope: &Scope) -> Surface {
    let tokens = approx_tokens(content);
    Surface {
        kind: "claude_md".to_string(),
        id: id.to_string(),
        scope: scope.clone(),
        config_path: config_path.to_string(),
        static_tokens: Some(tokens),
        startup_tokens: Some(tokens),
        load_mode: LoadMode::StartupFull,
    }
}

/// The YAML frontmatter block of a markdown file, without its `---` fences.
/// The block ends at the first line that is *only* dashes: a substring search
/// would also stop at a line merely starting with them, dropping every key
/// below it — and a lost `paths:` changes a rule's load mode, not just its
/// weight.
fn frontmatter(content: &str) -> Option<&str> {
    let opened = content.trim_start().strip_prefix("---")?;
    let body = &opened[opened.find('\n')? + 1..];
    let mut end = 0;
    for line in body.split_inclusive('\n') {
        if line.trim() == "---" {
            return Some(&body[..end]);
        }
        end += line.len();
    }
    None
}

/// Whether a markdown file's YAML frontmatter declares a `paths:` key.
fn has_paths_frontmatter(content: &str) -> bool {
    frontmatter(content).is_some_and(|block| {
        block
            .lines()
            .any(|line| line.trim_start().starts_with("paths:"))
    })
}

/// The frontmatter `description` — the text a skill or agent contributes to
/// every session's startup listing. Parsed defensively: a plain or quoted
/// scalar, or a block scalar, in every case folded together with the indented
/// continuation lines that follow it — YAML wraps a value onto those lines with
/// or without a `>` / `|` indicator, and reading only the first line would
/// understate the startup cost. A file with no frontmatter, or none declaring
/// the key, yields `None`.
fn frontmatter_description(content: &str) -> Option<String> {
    let mut lines = frontmatter(content)?.lines();
    let first = loop {
        // Only a top-level key is the surface's own description; an indented
        // `description:` belongs to some nested mapping.
        match lines.next()?.strip_prefix("description:") {
            Some(value) => break value.trim(),
            None => continue,
        }
    };
    // A block indicator is syntax, not text; anything else on the line is the
    // value's first fragment. Inside a block scalar (or quotes) a `#` is literal
    // text, so comments are only stripped from a plain scalar.
    let block = first.starts_with('>') || first.starts_with('|');
    let mut folded: Vec<&str> = Vec::new();
    if !(first.is_empty() || block) {
        folded.push(strip_comment(first, block));
    }
    folded.extend(
        lines
            .take_while(|line| line.trim().is_empty() || line.starts_with([' ', '\t']))
            .map(|line| strip_comment(line.trim(), block))
            .filter(|line| !line.is_empty()),
    );
    if folded.is_empty() {
        return None;
    }
    // Unquote the folded value, not a fragment of it: a quoted scalar's closing
    // quote may sit on the last continuation line.
    let value = folded.join(" ");
    Some(unquote(&value).to_string())
}

/// Drop a plain scalar's trailing YAML comment — a `#` that opens the value or
/// follows whitespace is comment syntax, and no session ever loads it. Inside a
/// block scalar (`block`) or quotes, a `#` is literal text and stays.
fn strip_comment(value: &str, block: bool) -> &str {
    if block || value.starts_with(['"', '\'']) {
        return value;
    }
    if value.starts_with('#') {
        return "";
    }
    // Any whitespace opens a comment, not only a space.
    value
        .char_indices()
        .find(|&(at, ch)| ch == '#' && value[..at].ends_with(char::is_whitespace))
        .map_or(value, |(at, _)| value[..at].trim_end())
}

/// Strip one layer of matching surrounding quotes, if present.
fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|v| v.strip_suffix(quote))
        {
            return inner;
        }
    }
    value
}

/// Read every `<name>.md` agent file in a directory into surfaces.
pub fn read_agent_surfaces(agents_dir: &Path, scope: &Scope) -> Vec<Surface> {
    read_markdown_files(agents_dir)
        .into_iter()
        .map(|(id, path, content)| agent_surface(&id, &path, &content, scope))
        .collect()
}

/// Read every `*.md` rule (recursively, so category subdirs are included) into
/// surfaces. The id is the path relative to the rules dir, without `.md`.
pub fn read_rule_surfaces(rules_dir: &Path, scope: &Scope) -> Vec<Surface> {
    let mut surfaces = Vec::new();
    collect_rule_surfaces(rules_dir, rules_dir, scope, &mut surfaces);
    surfaces
}

fn collect_rule_surfaces(root: &Path, dir: &Path, scope: &Scope, out: &mut Vec<Surface>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rule_surfaces(root, &path, scope, out);
        } else if path.extension().is_some_and(|ext| ext == "md")
            && let Ok(content) = fs::read_to_string(&path)
        {
            let id = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .with_extension("")
                .to_string_lossy()
                .into_owned();
            out.push(rule_surface(
                &id,
                &path.display().to_string(),
                &content,
                scope,
            ));
        }
    }
}

/// Read a single `CLAUDE.md` / `AGENTS.md` file into a surface, if it exists.
pub fn read_claude_md_surface(path: &Path, id: &str, scope: &Scope) -> Option<Surface> {
    let content = fs::read_to_string(path).ok()?;
    Some(claude_md_surface(
        id,
        &path.display().to_string(),
        &content,
        scope,
    ))
}

/// Read one project's installed config into surfaces, every one stamped with
/// that project's scope. Mirrors the global layout under `<root>/.claude`, plus
/// the in-repo `CLAUDE.md` / `AGENTS.md` and `.mcp.json`
/// (`docs/specs/config-format.md`). A root with none of these yields nothing.
pub fn read_project_surfaces(root: &Path, project: &str) -> Vec<Surface> {
    let scope = Scope::Project(project.to_string());
    let claude = root.join(".claude");
    let mut surfaces = read_skill_surfaces(&claude.join("skills"), &scope);
    surfaces.extend(read_rule_surfaces(&claude.join("rules"), &scope));
    surfaces.extend(read_agent_surfaces(&claude.join("agents"), &scope));
    surfaces.extend(read_mcp_server_surfaces(&root.join(".mcp.json"), &scope));
    // A repo that serves both agent conventions usually ships `AGENTS.md` as a
    // symlink to `CLAUDE.md`. Claude Code injects that content once, so weigh
    // the file once too: resolve each candidate and keep the first surface per
    // resolved file, which makes `CLAUDE.md` the one that is reported. Counting
    // both would double the project's always-on figure and suggest deleting a
    // link that costs nothing.
    let mut seen = HashSet::new();
    for name in ["CLAUDE.md", "AGENTS.md"] {
        let path = root.join(name);
        let resolved = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if !seen.insert(resolved) {
            continue;
        }
        if let Some(surface) = read_claude_md_surface(&path, name, &scope) {
            surfaces.push(surface);
        }
    }
    surfaces
}

/// Read MCP server declarations from an `mcp.json` (top-level `mcpServers`
/// object) into surfaces. The tool-schema cost is dynamic and not on disk, so
/// `static_tokens` is unknown (`None`) — see `docs/specs/config-format.md`.
pub fn read_mcp_server_surfaces(mcp_json: &Path, scope: &Scope) -> Vec<Surface> {
    let Ok(text) = fs::read_to_string(mcp_json) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    let Some(servers) = value.get("mcpServers").and_then(|v| v.as_object()) else {
        return Vec::new();
    };
    servers
        .keys()
        .map(|name| Surface {
            kind: "mcp_server".to_string(),
            // The catalog id must match the usage id, which is extracted from a
            // tool name (`mcp__<server>__…`) where Claude Code has already
            // sanitized the server name. Apply the same sanitization here so a
            // server like `grafana:prod` joins its `grafana_prod` usage instead
            // of showing up as both UNUSED (catalog) and ORPHANED (usage).
            id: sanitize_mcp_server_id(name),
            scope: scope.clone(),
            config_path: mcp_json.display().to_string(),
            static_tokens: None,
            // Its schema is loaded every session, but the weight is unknowable
            // from local config — unknown, never a false zero.
            startup_tokens: None,
            load_mode: LoadMode::ToolSchema,
        })
        .collect()
}

/// Sanitize an MCP server name to the id form Claude Code uses in tool names:
/// characters outside `[A-Za-z0-9_-]` (notably `:`) become `_`.
fn sanitize_mcp_server_id(name: &str) -> String {
    name.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

/// Read every `<name>.md` (recursively) as `(id, path, content)`, id being the
/// file stem.
fn read_markdown_files(dir: &Path) -> Vec<(String, String, String)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "md") {
                let id = path.file_stem()?.to_str()?.to_string();
                let content = fs::read_to_string(&path).ok()?;
                Some((id, path.display().to_string(), content))
            } else {
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approx_tokens_is_about_four_chars_each() {
        assert_eq!(approx_tokens(""), 0);
        assert_eq!(approx_tokens("abcd"), 1);
        assert_eq!(approx_tokens("abcde"), 2); // rounds up
    }

    #[test]
    fn skill_surface_weighs_the_whole_file_and_loads_by_description() {
        let surface = skill_surface(
            "git-commit",
            "/tmp/skills/git-commit/SKILL.md",
            "12345678", // 8 chars -> 2 tokens
            &Scope::Global,
        );

        assert_eq!(surface.kind, "skill");
        assert_eq!(surface.id, "git-commit");
        assert_eq!(surface.scope, Scope::Global);
        assert_eq!(surface.static_tokens, Some(2));
        assert_eq!(surface.load_mode, LoadMode::StartupDescription);
    }

    #[test]
    fn a_skill_pays_only_its_description_at_startup() {
        // The body is loaded on invocation, so the startup cost is the
        // description alone — 25 chars, ~7 tokens — not the 1000-char file.
        let content = format!(
            "---\nname: repro\ndescription: short startup description\n---\n{}",
            "x".repeat(1000)
        );
        let surface = skill_surface(
            "repro",
            "/tmp/skills/repro/SKILL.md",
            &content,
            &Scope::Global,
        );

        assert_eq!(surface.startup_tokens, Some(7));
        assert_eq!(surface.static_tokens, Some(approx_tokens(&content)));
    }

    #[test]
    fn a_description_less_skill_costs_nothing_at_startup() {
        let surface = skill_surface(
            "bare",
            "/tmp/skills/bare/SKILL.md",
            "# body",
            &Scope::Global,
        );
        assert_eq!(surface.startup_tokens, Some(0));
    }

    #[test]
    fn a_quoted_or_folded_description_is_read_like_any_other() {
        let quoted = "---\ndescription: \"abcdefgh\"\n---\nbody";
        assert_eq!(frontmatter_description(quoted).as_deref(), Some("abcdefgh"));
        // A folded block scalar spreads the description over the indented lines
        // that follow; all of it is startup-loaded text.
        let folded = "---\ndescription: >-\n  abcd\n  efgh\n---\nbody";
        assert_eq!(
            frontmatter_description(folded).as_deref(),
            Some("abcd efgh")
        );
        // No frontmatter at all, and a frontmatter without the key.
        assert_eq!(frontmatter_description("# body"), None);
        assert_eq!(frontmatter_description("---\nname: x\n---\nbody"), None);
    }

    #[test]
    fn a_wrapped_description_keeps_its_continuation_lines() {
        // A plain scalar needs no `>` to wrap: YAML folds the indented lines
        // that follow into one value, and all of it is startup-loaded text.
        // Reading only the first line would understate the startup cost.
        let wrapped = "---\ndescription: abcd\n  efgh\n---\nbody";
        assert_eq!(
            frontmatter_description(wrapped).as_deref(),
            Some("abcd efgh")
        );
        // A quoted value may span lines too — the quotes belong to the folded
        // value, not to its first fragment.
        let quoted = "---\ndescription: \"abcd\n  efgh\"\n---\nbody";
        assert_eq!(
            frontmatter_description(quoted).as_deref(),
            Some("abcd efgh")
        );
    }

    #[test]
    fn only_a_line_that_is_just_dashes_closes_the_frontmatter() {
        // A value line merely starting with dashes must not end the block:
        // truncating there drops the keys below it, and for a rule that means
        // losing `paths:` — which flips its load mode, not just its weight.
        let content = "---\nname: x\n---nope\ndescription: abcd\n---\nbody";
        assert_eq!(frontmatter_description(content).as_deref(), Some("abcd"));
        let ruled = "---\n---nope\npaths:\n  - \"src/**\"\n---\nbody";
        assert_eq!(
            rule_surface("r", "/c/r.md", ruled, &Scope::Global).load_mode,
            LoadMode::PathConditional
        );
        // CRLF frontmatter reads the same.
        let crlf = "---\r\ndescription: abcd\r\n---\r\nbody";
        assert_eq!(frontmatter_description(crlf).as_deref(), Some("abcd"));
    }

    #[test]
    fn an_inline_comment_is_not_part_of_the_description() {
        // Claude Code loads what YAML says the value is, and YAML drops a
        // whitespace-preceded `#` — counting it would weigh text no session
        // ever sees.
        let commented = "---\ndescription: Real text # not the description\n---\nbody";
        assert_eq!(
            frontmatter_description(commented).as_deref(),
            Some("Real text")
        );
        // Any whitespace opens a comment, not only a space.
        let tabbed = "---\ndescription: Real text\t# not the description\n---\nbody";
        assert_eq!(
            frontmatter_description(tabbed).as_deref(),
            Some("Real text")
        );
        // Inside quotes, and inside a block scalar, a `#` is literal text.
        let quoted = "---\ndescription: \"a # b\"\n---\nbody";
        assert_eq!(frontmatter_description(quoted).as_deref(), Some("a # b"));
        let block = "---\ndescription: |\n  a # b\n---\nbody";
        assert_eq!(frontmatter_description(block).as_deref(), Some("a # b"));
    }

    #[test]
    fn an_agent_pays_only_its_description_at_startup() {
        let content = "---\ndescription: abcdefgh\n---\nlong body";
        let surface = agent_surface("explorer", "/c/explorer.md", content, &Scope::Global);
        assert_eq!(surface.startup_tokens, Some(2));
        assert_eq!(surface.static_tokens, Some(approx_tokens(content)));
    }

    #[test]
    fn an_always_on_rule_pays_its_whole_text_at_startup() {
        let surface = rule_surface("convention", "/cfg/convention.md", "abcd", &Scope::Global);
        assert_eq!(surface.startup_tokens, Some(1));
        assert_eq!(surface.static_tokens, Some(1));
    }

    #[test]
    fn a_path_conditional_rule_pays_nothing_until_it_fires() {
        let content = "---\npaths:\n  - \"src/**/*.rs\"\n---\n# Rule body";
        let surface = rule_surface("spec-sync", "/cfg/spec-sync.md", content, &Scope::Global);
        assert_eq!(surface.startup_tokens, Some(0));
        assert_eq!(surface.static_tokens, Some(approx_tokens(content)));
    }

    #[test]
    fn claude_md_pays_its_whole_text_at_startup() {
        let surface = claude_md_surface("global", "/c/CLAUDE.md", "abcd", &Scope::Global);
        assert_eq!(surface.startup_tokens, Some(1));
    }

    #[test]
    fn an_mcp_server_startup_cost_is_unknown_not_zero() {
        let dir = std::env::temp_dir().join(format!("cclens-test-mcp-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".mcp.json");
        fs::write(&path, r#"{"mcpServers":{"playwright":{}}}"#).unwrap();

        let surfaces = read_mcp_server_surfaces(&path, &Scope::Global);
        fs::remove_dir_all(&dir).ok();

        assert_eq!(surfaces[0].startup_tokens, None);
    }

    #[test]
    fn a_rule_with_paths_frontmatter_is_path_conditional() {
        let content = "---\npaths:\n  - \"src/**/*.rs\"\n---\n# Rule body";
        let surface = rule_surface("spec-sync", "/cfg/spec-sync.md", content, &Scope::Global);
        assert_eq!(surface.kind, "rule");
        assert_eq!(surface.load_mode, LoadMode::PathConditional);
    }

    #[test]
    fn a_rule_without_paths_is_always_on() {
        let content = "---\ndescription: a thing\n---\n# Body";
        let surface = rule_surface("convention", "/cfg/convention.md", content, &Scope::Global);
        assert_eq!(surface.load_mode, LoadMode::StartupFull);
    }

    #[test]
    fn a_rule_with_no_frontmatter_is_always_on() {
        let surface = rule_surface("plain", "/cfg/plain.md", "# Just a heading", &Scope::Global);
        assert_eq!(surface.load_mode, LoadMode::StartupFull);
    }

    #[test]
    fn mcp_server_id_is_sanitized_to_match_tool_name_usage() {
        // A `grafana:prod` config key must become `grafana_prod` so it joins the
        // usage extracted from `mcp__grafana_prod__...` tool names.
        assert_eq!(sanitize_mcp_server_id("grafana:prod"), "grafana_prod");
        assert_eq!(
            sanitize_mcp_server_id("grafana:unofficial-economy-production"),
            "grafana_unofficial-economy-production"
        );
        // Already-clean names are unchanged.
        assert_eq!(sanitize_mcp_server_id("playwright"), "playwright");
    }

    #[test]
    fn read_project_surfaces_walks_the_project_layout() {
        // A synthetic project directory (privacy rule: fixtures are fabricated).
        let root = std::env::temp_dir().join(format!("cclens-test-project-{}", std::process::id()));
        let claude = root.join(".claude");
        fs::create_dir_all(claude.join("skills/deploy")).unwrap();
        fs::write(claude.join("skills/deploy/SKILL.md"), "deploy skill").unwrap();
        fs::create_dir_all(claude.join("rules")).unwrap();
        fs::write(claude.join("rules/tdd.md"), "# tdd").unwrap();
        fs::write(root.join("CLAUDE.md"), "project claude md").unwrap();
        fs::write(
            root.join(".mcp.json"),
            r#"{"mcpServers":{"playwright":{}}}"#,
        )
        .unwrap();

        let surfaces = read_project_surfaces(&root, "alpha");
        fs::remove_dir_all(&root).ok();

        let has = |kind: &str, id: &str| surfaces.iter().any(|s| s.kind == kind && s.id == id);
        assert!(
            surfaces
                .iter()
                .all(|s| s.scope == Scope::Project("alpha".to_string()))
        );
        assert!(has("skill", "deploy"));
        assert!(has("rule", "tdd"));
        assert!(has("claude_md", "CLAUDE.md"));
        assert!(has("mcp_server", "playwright"));
    }

    #[cfg(unix)]
    #[test]
    fn an_agents_md_symlinked_to_claude_md_is_one_surface() {
        // A repo serving both agent conventions ships AGENTS.md as a symlink to
        // CLAUDE.md. Claude Code injects that content once, so counting both
        // paths would double the always-on figure and suggest deleting a link
        // that costs nothing.
        let root = std::env::temp_dir().join(format!(
            "cclens-test-symlink-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        // Start from an empty directory: a leftover AGENTS.md from an aborted
        // run would make the symlink below fail with AlreadyExists.
        fs::remove_dir_all(&root).ok();
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("CLAUDE.md"), "project claude md").unwrap();
        std::os::unix::fs::symlink("CLAUDE.md", root.join("AGENTS.md")).unwrap();

        let surfaces = read_project_surfaces(&root, "alpha");
        fs::remove_dir_all(&root).ok();

        let claude_mds: Vec<_> = surfaces.iter().filter(|s| s.kind == "claude_md").collect();
        assert_eq!(claude_mds.len(), 1);
        assert_eq!(claude_mds[0].id, "CLAUDE.md");
    }

    #[cfg(unix)]
    #[test]
    fn an_agents_md_of_its_own_stays_a_separate_surface() {
        // Only the *same file* is deduplicated: a standalone AGENTS.md is real
        // extra always-on context.
        let root = std::env::temp_dir().join(format!(
            "cclens-test-distinct-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        // Start from an empty directory: a leftover symlinked AGENTS.md from an
        // aborted run would write through to CLAUDE.md and hide the two files.
        fs::remove_dir_all(&root).ok();
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("CLAUDE.md"), "project claude md").unwrap();
        fs::write(root.join("AGENTS.md"), "project agents md").unwrap();

        let surfaces = read_project_surfaces(&root, "alpha");
        fs::remove_dir_all(&root).ok();

        let mut ids: Vec<_> = surfaces
            .iter()
            .filter(|s| s.kind == "claude_md")
            .map(|s| s.id.as_str())
            .collect();
        ids.sort();
        assert_eq!(ids, ["AGENTS.md", "CLAUDE.md"]);
    }

    #[test]
    fn claude_md_is_always_on_and_agents_load_by_description() {
        assert_eq!(
            claude_md_surface("global", "/c/CLAUDE.md", "x", &Scope::Global).load_mode,
            LoadMode::StartupFull
        );
        assert_eq!(
            agent_surface("explorer", "/c/explorer.md", "x", &Scope::Global).load_mode,
            LoadMode::StartupDescription
        );
    }
}
