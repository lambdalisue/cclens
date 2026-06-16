//! Parse Claude Code config into the surface catalog. Like the transcript
//! adapter, the parsing core is pure (over file content) and the directory
//! walking is a thin shell. See `docs/specs/config-format.md`.

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
/// description at startup, so the load mode is `StartupDescription`; the static
/// cost is the whole file (what is paid on-demand when invoked).
pub fn skill_surface(id: &str, config_path: &str, content: &str, scope: Scope) -> Surface {
    Surface {
        kind: "skill".to_string(),
        id: id.to_string(),
        scope,
        config_path: config_path.to_string(),
        static_tokens: Some(approx_tokens(content)),
        load_mode: LoadMode::StartupDescription,
    }
}

/// Read every `<name>/SKILL.md` under a skills directory into surfaces. A
/// missing directory yields nothing (the scope may simply not exist).
pub fn read_skill_surfaces(skills_dir: &Path, scope: Scope) -> Vec<Surface> {
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
pub fn rule_surface(id: &str, config_path: &str, content: &str, scope: Scope) -> Surface {
    let load_mode = if has_paths_frontmatter(content) {
        LoadMode::PathConditional
    } else {
        LoadMode::StartupFull
    };
    Surface {
        kind: "rule".to_string(),
        id: id.to_string(),
        scope,
        config_path: config_path.to_string(),
        static_tokens: Some(approx_tokens(content)),
        load_mode,
    }
}

/// Build an `agent` surface. Like skills, only the description loads at startup.
pub fn agent_surface(id: &str, config_path: &str, content: &str, scope: Scope) -> Surface {
    Surface {
        kind: "agent".to_string(),
        id: id.to_string(),
        scope,
        config_path: config_path.to_string(),
        static_tokens: Some(approx_tokens(content)),
        load_mode: LoadMode::StartupDescription,
    }
}

/// Build a `claude_md` surface — always-on context paid every session.
pub fn claude_md_surface(id: &str, config_path: &str, content: &str, scope: Scope) -> Surface {
    Surface {
        kind: "claude_md".to_string(),
        id: id.to_string(),
        scope,
        config_path: config_path.to_string(),
        static_tokens: Some(approx_tokens(content)),
        load_mode: LoadMode::StartupFull,
    }
}

/// Whether a markdown file's YAML frontmatter declares a `paths:` key.
fn has_paths_frontmatter(content: &str) -> bool {
    let Some(rest) = content.trim_start().strip_prefix("---") else {
        return false;
    };
    let Some(end) = rest.find("\n---") else {
        return false;
    };
    rest[..end]
        .lines()
        .any(|line| line.trim_start().starts_with("paths:"))
}

/// Read every `<name>.md` agent file in a directory into surfaces.
pub fn read_agent_surfaces(agents_dir: &Path, scope: Scope) -> Vec<Surface> {
    read_markdown_files(agents_dir)
        .into_iter()
        .map(|(id, path, content)| agent_surface(&id, &path, &content, scope))
        .collect()
}

/// Read every `*.md` rule (recursively, so category subdirs are included) into
/// surfaces. The id is the path relative to the rules dir, without `.md`.
pub fn read_rule_surfaces(rules_dir: &Path, scope: Scope) -> Vec<Surface> {
    let mut surfaces = Vec::new();
    collect_rule_surfaces(rules_dir, rules_dir, scope, &mut surfaces);
    surfaces
}

fn collect_rule_surfaces(root: &Path, dir: &Path, scope: Scope, out: &mut Vec<Surface>) {
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
pub fn read_claude_md_surface(path: &Path, id: &str, scope: Scope) -> Option<Surface> {
    let content = fs::read_to_string(path).ok()?;
    Some(claude_md_surface(
        id,
        &path.display().to_string(),
        &content,
        scope,
    ))
}

/// Read MCP server declarations from an `mcp.json` (top-level `mcpServers`
/// object) into surfaces. The tool-schema cost is dynamic and not on disk, so
/// `static_tokens` is unknown (`None`) — see `docs/specs/config-format.md`.
pub fn read_mcp_server_surfaces(mcp_json: &Path, scope: Scope) -> Vec<Surface> {
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
            id: name.clone(),
            scope,
            config_path: mcp_json.display().to_string(),
            static_tokens: None,
            load_mode: LoadMode::ToolSchema,
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
            Scope::Global,
        );

        assert_eq!(surface.kind, "skill");
        assert_eq!(surface.id, "git-commit");
        assert_eq!(surface.scope, Scope::Global);
        assert_eq!(surface.static_tokens, Some(2));
        assert_eq!(surface.load_mode, LoadMode::StartupDescription);
    }

    #[test]
    fn a_rule_with_paths_frontmatter_is_path_conditional() {
        let content = "---\npaths:\n  - \"src/**/*.rs\"\n---\n# Rule body";
        let surface = rule_surface("spec-sync", "/cfg/spec-sync.md", content, Scope::Global);
        assert_eq!(surface.kind, "rule");
        assert_eq!(surface.load_mode, LoadMode::PathConditional);
    }

    #[test]
    fn a_rule_without_paths_is_always_on() {
        let content = "---\ndescription: a thing\n---\n# Body";
        let surface = rule_surface("convention", "/cfg/convention.md", content, Scope::Global);
        assert_eq!(surface.load_mode, LoadMode::StartupFull);
    }

    #[test]
    fn a_rule_with_no_frontmatter_is_always_on() {
        let surface = rule_surface("plain", "/cfg/plain.md", "# Just a heading", Scope::Global);
        assert_eq!(surface.load_mode, LoadMode::StartupFull);
    }

    #[test]
    fn claude_md_is_always_on_and_agents_load_by_description() {
        assert_eq!(
            claude_md_surface("global", "/c/CLAUDE.md", "x", Scope::Global).load_mode,
            LoadMode::StartupFull
        );
        assert_eq!(
            agent_surface("explorer", "/c/explorer.md", "x", Scope::Global).load_mode,
            LoadMode::StartupDescription
        );
    }
}
