//! When each file-backed config surface last changed — the boundary of the
//! before/after comparison (`core::effect`). Git history of the file's real
//! path is the evidence; the mtime is only the fallback, because symlink
//! chains, checkouts and Nix stores rewrite it (`docs/specs/config-format.md`
//! "Change dates").

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::UNIX_EPOCH;

use crate::adapter::config::frontmatter_targets;
use crate::core::effect::{ConfigChange, DateSource};
use crate::core::friction::ErrorCategory;
use crate::core::surface::{Scope, Surface};

/// The surface kinds compared: those whose `config_path` is the surface's own
/// file and whose effect shows on the main thread, where the failures are read.
/// An MCP server's path is the `mcp.json` every server shares, so a date read
/// from it would belong to all of them at once; an agent acts in its own
/// subagent transcript, whose failures the comparison does not see.
const COMPARED: [&str; 3] = ["skill", "rule", "claude_md"];

/// A change record for every file-backed surface whose file can still be read.
/// A project row whose real file is also catalogued globally is dropped (a
/// project file symlinked to a global one, say): the global row already
/// compares every session, that project's included. Project rows sharing a file with no global copy each stay, since
/// each governs its own project's sessions.
pub fn config_changes(surfaces: &[Surface]) -> Vec<ConfigChange> {
    let real = |s: &Surface| fs::canonicalize(&s.config_path).ok();
    let global_files: std::collections::HashSet<_> = surfaces
        .iter()
        .filter(|s| s.scope == Scope::Global)
        .filter_map(real)
        .collect();
    surfaces
        .iter()
        .filter(|s| COMPARED.contains(&s.kind.as_str()))
        .filter(|s| {
            s.scope == Scope::Global || real(s).is_none_or(|path| !global_files.contains(&path))
        })
        .filter_map(|s| {
            let path = Path::new(&s.config_path);
            let (changed_epoch, introduced_epoch, date_source) = file_dates(path)?;
            let targets = fs::read_to_string(path)
                .map(|content| frontmatter_targets(&content))
                .unwrap_or_default();
            Some(ConfigChange {
                kind: s.kind.clone(),
                id: s.id.clone(),
                scope: s.scope.clone(),
                changed_epoch,
                introduced_epoch,
                date_source,
                targets,
                always_on: s.load_mode.is_always_on(),
            })
        })
        .collect()
}

/// `(last change, introduction, provenance)` for one file, or `None` when it
/// cannot be read at all.
pub fn file_dates(path: &Path) -> Option<(i64, i64, DateSource)> {
    let real = fs::canonicalize(path).ok()?;
    let mtime = fs::metadata(&real)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    let (dir, name) = (real.parent()?, real.file_name()?);
    let git = |args: &[&str]| -> Option<String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .arg("--")
            .arg(name)
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    };
    const DIFF: [&str; 3] = ["-U0", "--no-color", "--no-ext-diff"];
    let log = {
        let mut args = vec!["log", "--follow", "-p", "--format=@@cclens %at"];
        args.extend(DIFF);
        git(&args)
    };
    let Some((changed, introduced)) = log.as_deref().and_then(parse_history) else {
        return Some((mtime, mtime, DateSource::Mtime));
    };
    let dirty = {
        let mut args = vec!["diff", "HEAD"];
        args.extend(DIFF);
        git(&args).is_some_and(|diff| is_substantive(&diff))
    };
    if dirty {
        Some((mtime.max(changed), introduced, DateSource::Uncommitted))
    } else {
        Some((changed, introduced, DateSource::Git))
    }
}

/// `(last change, introduction)` from `git log -p --format='@@cclens %at'`,
/// newest commit first. The last change is the newest commit that changed the
/// rule itself rather than only its `cclens-targets:` declaration — declaring a
/// rule must not erase its own before-window. Commits are taken in history
/// order, not by largest time: a cherry-pick keeps its original author time.
/// `None` when the file has no history.
fn parse_history(log: &str) -> Option<(i64, i64)> {
    let mut commits: Vec<(i64, String)> = Vec::new();
    for line in log.lines() {
        if let Some(time) = line.strip_prefix("@@cclens ") {
            commits.push((time.trim().parse().ok()?, String::new()));
        } else if let Some((_, diff)) = commits.last_mut() {
            diff.push_str(line);
            diff.push('\n');
        }
    }
    let introduced = commits.last()?.0;
    let changed = commits
        .iter()
        .find(|(_, diff)| is_substantive(diff))
        .unwrap_or(&commits[0])
        .0;
    Some((changed, introduced))
}

/// Whether a unified diff touches anything beyond a `cclens-targets:`
/// declaration and the frontmatter fences and blank lines that hold it.
fn is_substantive(diff: &str) -> bool {
    let mut in_hunk = false;
    diff.lines().any(|line| {
        if line.starts_with("diff --git") {
            in_hunk = false;
        } else if line.starts_with("@@ ") {
            in_hunk = true;
        } else if in_hunk && let Some(content) = line.strip_prefix(['+', '-']) {
            return !is_declaration_line(content);
        }
        false
    })
}

fn is_declaration_line(content: &str) -> bool {
    let line = content.trim();
    let is_category = |item: &str| {
        let item = item.trim().trim_matches(['"', '\'']);
        ErrorCategory::from_label(item).label() == item
    };
    line.is_empty()
        || line == "---"
        || line.starts_with("cclens-targets:")
        || line.strip_prefix("- ").is_some_and(is_category)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn the_newest_commit_is_the_change_and_the_oldest_the_introduction() {
        let log = "@@cclens 300\n\n@@ -1 +1 @@\n+b\n@@cclens 200\n\n@@ -1 +0,0 @@\n-a\n@@cclens 100\n\n@@ -0,0 +1 @@\n+a\n";
        assert_eq!(parse_history(log), Some((300, 100)));
        assert_eq!(parse_history(""), None);
        // A cherry-pick keeps its original author time: the newest commit in
        // history order is still the change, even when an older one has a
        // later author time.
        let picked = "@@cclens 150\n\n@@ -1 +1 @@\n+b\n@@cclens 200\n\n@@ -1 +1 @@\n+a\n@@cclens 100\n\n@@ -0,0 +1 @@\n+x\n";
        assert_eq!(parse_history(picked), Some((150, 100)));
    }

    #[test]
    fn adding_a_declaration_is_not_a_change_of_the_rule() {
        // Declaring what a rule targets is metadata about it: were it to move
        // the boundary, declaring a rule would erase its own before-window.
        let log = "@@cclens 300\n\ndiff --git a/r.md b/r.md\n--- a/r.md\n+++ b/r.md\n\
                   @@ -0,0 +1,4 @@\n+---\n+cclens-targets: [timeout]\n+---\n+\n\
                   @@cclens 200\n\n@@ -0,0 +1 @@\n+body\n";
        assert_eq!(parse_history(log), Some((200, 200)));
        let block = "@@ -1,2 +1,4 @@\n cclens-targets:\n+  - timeout\n+  - path-not-found\n";
        assert!(!is_substantive(block));
        assert!(is_substantive("@@ -3 +3 @@\n-old\n+new\n"));
        // A bullet in the body that happens to read like a word is still body.
        assert!(is_substantive("@@ -3 +3 @@\n+- remember this\n"));
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cclens-history-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn git(dir: &Path, args: &[&str], date: &str) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .env("GIT_AUTHOR_DATE", date)
            .env("GIT_COMMITTER_DATE", date)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn a_committed_file_is_dated_by_its_history_not_its_mtime() {
        let dir = scratch_dir("git");
        git(&dir, &["init", "-q"], "@0");
        fs::write(dir.join("rule.md"), "one").unwrap();
        git(&dir, &["add", "rule.md"], "@0");
        git(&dir, &["commit", "-q", "-m", "add"], "@1000 +0000");
        fs::write(dir.join("rule.md"), "two").unwrap();
        git(&dir, &["commit", "-q", "-am", "rework"], "@2000 +0000");
        assert_eq!(
            file_dates(&dir.join("rule.md")),
            Some((2000, 1000, DateSource::Git))
        );
        // A symlink into the checkout resolves to the same history.
        #[cfg(unix)]
        {
            let link = dir.join("link.md");
            std::os::unix::fs::symlink(dir.join("rule.md"), &link).unwrap();
            assert_eq!(file_dates(&link), Some((2000, 1000, DateSource::Git)));
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_uncommitted_edit_is_dated_by_its_mtime() {
        let dir = scratch_dir("dirty");
        git(&dir, &["init", "-q"], "@0");
        fs::write(dir.join("rule.md"), "one").unwrap();
        git(&dir, &["add", "rule.md"], "@0");
        git(&dir, &["commit", "-q", "-m", "add"], "@1000 +0000");
        fs::write(dir.join("rule.md"), "edited").unwrap();
        let (changed, introduced, source) = file_dates(&dir.join("rule.md")).unwrap();
        assert_eq!((introduced, source), (1000, DateSource::Uncommitted));
        assert!(changed > 1000);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_declaration_committed_or_not_keeps_the_rule_dated_by_its_body() {
        let dir = scratch_dir("declared");
        git(&dir, &["init", "-q"], "@0");
        fs::write(dir.join("rule.md"), "# Rule\n\nbody\n").unwrap();
        git(&dir, &["add", "rule.md"], "@0");
        git(&dir, &["commit", "-q", "-m", "add"], "@1000 +0000");
        let declared = "---\ncclens-targets: [timeout]\n---\n\n# Rule\n\nbody\n";
        fs::write(dir.join("rule.md"), declared).unwrap();
        assert_eq!(
            file_dates(&dir.join("rule.md")),
            Some((1000, 1000, DateSource::Git))
        );
        git(&dir, &["commit", "-q", "-am", "declare"], "@2000 +0000");
        assert_eq!(
            file_dates(&dir.join("rule.md")),
            Some((1000, 1000, DateSource::Git))
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn one_file_catalogued_twice_is_one_change_under_its_first_scope() {
        use crate::core::surface::{LoadMode, Scope};
        let dir = scratch_dir("twice");
        fs::write(dir.join("rule.md"), "---\ncclens-targets: [timeout]\n---\n").unwrap();
        let surface = |scope: Scope| Surface {
            kind: "rule".to_string(),
            id: "rule".to_string(),
            scope,
            config_path: dir.join("rule.md").display().to_string(),
            static_tokens: Some(1),
            startup_tokens: Some(1),
            load_mode: LoadMode::StartupFull,
            attrs: Vec::new(),
        };
        let changes = config_changes(&[
            surface(Scope::Global),
            surface(Scope::Project("example".to_string())),
        ]);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].scope, Scope::Global);
        assert_eq!(changes[0].targets, ["timeout"]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn projects_sharing_a_file_without_a_global_copy_each_keep_their_change() {
        use crate::core::surface::{LoadMode, Scope};
        let dir = scratch_dir("shared");
        fs::write(dir.join("rule.md"), "x").unwrap();
        let surface = |project: &str| Surface {
            kind: "rule".to_string(),
            id: "rule".to_string(),
            scope: Scope::Project(project.to_string()),
            config_path: dir.join("rule.md").display().to_string(),
            static_tokens: Some(1),
            startup_tokens: Some(1),
            load_mode: LoadMode::StartupFull,
            attrs: Vec::new(),
        };
        let changes = config_changes(&[surface("alpha"), surface("beta")]);
        let scopes: Vec<&Scope> = changes.iter().map(|c| &c.scope).collect();
        assert_eq!(
            scopes,
            [
                &Scope::Project("alpha".to_string()),
                &Scope::Project("beta".to_string())
            ]
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_agent_is_not_compared_on_main_thread_failures() {
        use crate::core::surface::{LoadMode, Scope};
        let dir = scratch_dir("agent");
        fs::write(dir.join("helper.md"), "x").unwrap();
        let agent = Surface {
            kind: "agent".to_string(),
            id: "helper".to_string(),
            scope: Scope::Global,
            config_path: dir.join("helper.md").display().to_string(),
            static_tokens: Some(1),
            startup_tokens: Some(1),
            load_mode: LoadMode::StartupDescription,
            attrs: Vec::new(),
        };
        assert!(config_changes(&[agent]).is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_file_without_history_falls_back_to_its_mtime() {
        let dir = scratch_dir("plain");
        fs::write(dir.join("rule.md"), "x").unwrap();
        let (changed, introduced, source) = file_dates(&dir.join("rule.md")).unwrap();
        assert_eq!(source, DateSource::Mtime);
        assert_eq!(changed, introduced);
        fs::remove_dir_all(&dir).ok();
    }
}
