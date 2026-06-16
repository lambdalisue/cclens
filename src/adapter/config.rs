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
}
