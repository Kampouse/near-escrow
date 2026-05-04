use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::{info, warn, error};

/// Manages Radicle operations: clone, checkout, cleanup.
pub struct RadicleClient {
    work_dir: PathBuf,
}

impl RadicleClient {
    pub fn new(work_dir: impl Into<PathBuf>) -> Self {
        Self { work_dir: work_dir.into() }
    }

    /// Clone a Radicle repo by RID. Returns the path to the cloned repo.
    pub fn clone_repo(&self, rid: &str) -> Result<PathBuf> {
        let repo_dir = self.work_dir.join(sanitize_rid(rid));

        if repo_dir.exists() {
            // Already cloned, just fetch latest
            info!("Repo already exists at {}, fetching...", repo_dir.display());
            self.run_rad(&repo_dir, &["sync", "--fetch"])?;
            return Ok(repo_dir);
        }

        info!("Cloning Radicle repo {} to {}...", rid, repo_dir.display());
        self.run_rad(&self.work_dir, &["clone", rid, &sanitize_rid(rid)])?;

        if !repo_dir.exists() {
            // rad clone might put it in a subdirectory
            let alt = self.work_dir.join(rid.replace("rad:", ""));
            if alt.exists() {
                return Ok(alt);
            }
            anyhow::bail!("Clone succeeded but repo dir not found at {}", repo_dir.display());
        }

        Ok(repo_dir)
    }

    /// Checkout a specific commit SHA in the cloned repo.
    pub fn checkout_commit(&self, repo_dir: &Path, commit_sha: &str) -> Result<()> {
        info!("Checking out commit {} in {}...", commit_sha, repo_dir.display());
        self.run_git(repo_dir, &["checkout", commit_sha])?;
        Ok(())
    }

    /// Checkout a specific branch.
    pub fn checkout_branch(&self, repo_dir: &Path, branch: &str) -> Result<()> {
        info!("Checking out branch {} in {}...", branch, repo_dir.display());
        self.run_git(repo_dir, &["checkout", branch])?;
        self.run_git(repo_dir, &["pull"])?;
        Ok(())
    }

    /// Get the current HEAD commit SHA.
    pub fn head_commit(&self, repo_dir: &Path) -> Result<String> {
        let output = self.run_git_with_output(repo_dir, &["rev-parse", "HEAD"])?;
        Ok(output.trim().to_string())
    }

    /// List branches in the repo.
    pub fn list_branches(&self, repo_dir: &Path) -> Result<Vec<String>> {
        let output = self.run_git_with_output(repo_dir, &["branch", "--format=%(refname:short)"])?;
        Ok(output.lines().map(|l| l.trim().to_string()).collect())
    }

    /// Read a file from the repo.
    pub fn read_file(&self, repo_dir: &Path, relative_path: &str) -> Result<String> {
        let full_path = repo_dir.join(relative_path);
        std::fs::read_to_string(&full_path)
            .with_context(|| format!("Failed to read {}", full_path.display()))
    }

    /// Check if verify/verify.sh exists in the repo.
    pub fn has_verify_script(&self, repo_dir: &Path) -> bool {
        repo_dir.join("verify").join("verify.sh").exists()
    }

    /// Check if MANIFEST.json exists in the repo.
    pub fn has_manifest(&self, repo_dir: &Path) -> bool {
        repo_dir.join("MANIFEST.json").exists()
    }

    /// Remove a cloned repo (cleanup after settlement).
    pub fn cleanup(&self, rid: &str) -> Result<()> {
        let repo_dir = self.work_dir.join(sanitize_rid(rid));
        if repo_dir.exists() {
            info!("Cleaning up repo at {}...", repo_dir.display());
            std::fs::remove_dir_all(&repo_dir)
                .with_context(|| format!("Failed to remove {}", repo_dir.display()))?;
        }
        Ok(())
    }

    fn run_rad(&self, cwd: &Path, args: &[&str]) -> Result<()> {
        let output = Command::new("rad")
            .args(args)
            .current_dir(cwd)
            .env("RAD_HOME", &self.work_dir)
            .output()
            .with_context(|| format!("Failed to run rad {}", args.join(" ")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("rad {} failed: {}", args.join(" "), stderr);
        }

        Ok(())
    }

    fn run_git(&self, cwd: &Path, args: &[&str]) -> Result<()> {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .with_context(|| format!("Failed to run git {}", args.join(" ")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("git {} failed: {}", args.join(" "), stderr);
        }

        Ok(())
    }

    fn run_git_with_output(&self, cwd: &Path, args: &[&str]) -> Result<String> {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .with_context(|| format!("Failed to run git {}", args.join(" ")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("git {} failed: {}", args.join(" "), stderr);
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }
}

/// Sanitize a Radicle RID to be a valid directory name.
fn sanitize_rid(rid: &str) -> String {
    rid.replace(":", "_").replace("/", "_")
}
