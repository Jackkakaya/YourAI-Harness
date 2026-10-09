//! Read-only git context for status display: workspace / worktree / branch.
//!
//! Detection is best-effort and purely informational: outside a repository
//! every field stays `None` and callers render nothing. The harness tracks
//! registered worktrees by name in `worktrees.json`; auto-created ones live
//! under `<session>/worktrees/<name>`, so the linked-worktree's toplevel
//! basename matches the registered name.
use std::path::Path;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitContext {
    /// Repository toplevel directory name; None outside a git repository.
    pub workspace: Option<String>,
    /// Linked-worktree name (toplevel basename); None in the main worktree.
    pub worktree: Option<String>,
    /// Branch name, or a short commit SHA when HEAD is detached.
    pub branch: Option<String>,
}

impl GitContext {
    /// Footer tag: the branch (or detached SHA); the path next to it already
    /// names the worktree directory when one is in use.
    pub fn footer_label(&self) -> Option<&str> {
        self.branch.as_deref()
    }

    /// One dashboard line describing the whole trio; None outside a repo.
    pub fn stats_line(&self) -> Option<String> {
        let workspace = self.workspace.as_deref()?;
        let mut line = format!("git {workspace}");
        if let Some(worktree) = &self.worktree {
            line.push_str(&format!(" · worktree {worktree}"));
        }
        if let Some(branch) = &self.branch {
            line.push_str(&format!(" · {branch}"));
        }
        Some(line)
    }

    /// Detect the context for a working directory. Two short-lived `git`
    /// processes at most (the second only for a detached HEAD).
    pub async fn detect(cwd: &Path) -> Self {
        // One call prints both roots: the current toplevel and the shared
        // git dir (whose parent is the main repository's root).
        let Some(roots) = output(
            cwd,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--show-toplevel",
                "--git-common-dir",
            ],
        )
        .await
        else {
            return Self::default();
        };
        let mut lines = roots.lines();
        let (Some(toplevel), Some(common_dir)) = (lines.next(), lines.next()) else {
            return Self::default();
        };
        let toplevel = Path::new(toplevel);
        let common_root = Path::new(common_dir).parent().unwrap_or(toplevel);
        let name = |p: &Path| {
            p.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        };
        // Inside a linked worktree the toplevel is a sibling of the main
        // repository root; harness worktrees are named by that basename.
        let (workspace, worktree) = if common_root == toplevel {
            (name(toplevel), None)
        } else {
            (name(common_root), Some(name(toplevel)))
        };
        if workspace.is_empty() {
            return Self::default();
        }
        // symbolic-ref resolves unborn branches too; detached HEAD falls back
        // to the short SHA, and an unborn detached state stays unnamed.
        let branch = match output(cwd, &["symbolic-ref", "--short", "HEAD"]).await {
            Some(branch) => Some(branch),
            _ => output(cwd, &["rev-parse", "--short", "HEAD"]).await,
        };
        Self {
            workspace: Some(workspace),
            worktree,
            branch,
        }
    }
}

/// Run `git` in `cwd` and return its trimmed stdout, or None on any failure.
async fn output(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = tokio::process::Command::new("git")
        .current_dir(cwd)
        .args(args)
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    #[allow(clippy::wildcard_imports)]
    use super::*;

    #[test]
    fn labels_render_only_what_is_known() {
        let none = GitContext::default();
        assert_eq!(none.footer_label(), None);
        assert_eq!(none.stats_line(), None);

        let main = GitContext {
            workspace: Some("YourAI-Harness".into()),
            worktree: None,
            branch: Some("main".into()),
        };
        assert_eq!(main.footer_label(), Some("main"));
        assert_eq!(
            main.stats_line().as_deref(),
            Some("git YourAI-Harness · main")
        );

        let worktree = GitContext {
            workspace: Some("YourAI-Harness".into()),
            worktree: Some("feat-x".into()),
            branch: Some("a1b2c3d".into()),
        };
        assert_eq!(worktree.footer_label(), Some("a1b2c3d"));
        assert_eq!(
            worktree.stats_line().as_deref(),
            Some("git YourAI-Harness · worktree feat-x · a1b2c3d")
        );
    }

    fn git(dir: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("git binary");
        assert!(
            ok.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&ok.stderr)
        );
    }

    #[tokio::test]
    async fn detects_branch_and_linked_worktree_in_a_real_repository() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("demo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-b", "main"]);
        git(&repo, &["config", "user.email", "t@t"]);
        git(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("a.txt"), "a").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "init"]);

        let ctx = GitContext::detect(&repo).await;
        assert_eq!(ctx.workspace.as_deref(), Some("demo"));
        assert_eq!(ctx.worktree, None);
        assert_eq!(ctx.branch.as_deref(), Some("main"));

        // A linked worktree: `.git` becomes a file and the harness names
        // worktrees by their directory basename. The workspace stays the
        // main repository, even from inside the worktree.
        let wt = dir.path().join("demo-wt");
        git(
            &repo,
            &["worktree", "add", "--detach", wt.to_str().unwrap(), "HEAD"],
        );
        let ctx = GitContext::detect(&wt).await;
        assert_eq!(ctx.workspace.as_deref(), Some("demo"));
        assert_eq!(ctx.worktree.as_deref(), Some("demo-wt"));
        // Detached HEAD: symbolic-ref fails, rev-parse supplies the SHA.
        assert!(ctx.branch.as_deref().is_some_and(|b| b.len() >= 7));

        // Outside any repository everything stays quiet.
        let bare = tempfile::tempdir().unwrap();
        let ctx = GitContext::detect(bare.path()).await;
        assert_eq!(ctx, GitContext::default());
    }
}
