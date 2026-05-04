use anyhow::{Context, Result};
use sha2::{Sha256, Digest};
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

    /// Checkout a Radicle patch by ID.
    /// Creates a local branch from the patch for inspection.
    /// Returns the commit SHA of the patch HEAD.
    pub fn checkout_patch(&self, repo_dir: &Path, patch_id: &str) -> Result<String> {
        info!("Checking out patch {} in {}...", patch_id, repo_dir.display());
        // `rad patch checkout` creates a local branch from the patch
        self.run_rad(repo_dir, &["patch", "checkout", patch_id])?;

        // Get the HEAD commit of the checked-out patch
        let head = self.head_commit(repo_dir)?;
        info!("Patch {} checked out at commit {}", patch_id, head);
        Ok(head)
    }

    /// Get the base commit (main/master HEAD) of the repo.
    /// Used to read verify/ from the agent's original branch.
    pub fn base_commit(&self, repo_dir: &Path) -> Result<String> {
        // Try main first, then master
        let output = self.run_git_with_output(repo_dir, &["rev-parse", "main"])?;
        let sha = output.trim().to_string();
        if !sha.is_empty() {
            return Ok(sha);
        }
        let output = self.run_git_with_output(repo_dir, &["rev-parse", "master"])?;
        Ok(output.trim().to_string())
    }

    /// Read a file from the repo at a specific commit (without checking out).
    pub fn read_file_at_commit(&self, repo_dir: &Path, commit_sha: &str, path: &str) -> Result<String> {
        let output = self.run_git_with_output(repo_dir, &["show", &format!("{}:{}", commit_sha, path)])?;
        Ok(output)
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

/// Compute a deterministic SHA-256 hash of all files in a directory.
/// Files are sorted by relative path, then each file's content is hashed
/// into a cumulative SHA-256. Returns hex-encoded hash with "sha256:" prefix.
pub fn hash_directory(dir: &Path) -> Result<String> {
    let mut all_files = Vec::new();
    collect_files(dir, dir, &mut all_files)?;
    all_files.sort();

    let mut hasher = Sha256::new();
    for (rel_path, full_path) in &all_files {
        // Feed relative path then file content into the hash
        hasher.update(rel_path.as_bytes());
        hasher.update(b"\0");
        let content = std::fs::read(full_path)
            .with_context(|| format!("Failed to read {}", full_path.display()))?;
        hasher.update(&content);
        hasher.update(b"\0");
    }

    let hash = hasher.finalize();
    Ok(format!("sha256:{:x}", hash))
}

/// Recursively collect all files under a directory.
fn collect_files(base: &Path, dir: &Path, files: &mut Vec<(String, PathBuf)>) -> Result<()> {
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("Failed to read dir {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_files(base, &path, files)?;
        } else {
            let rel = path
                .strip_prefix(base)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();
            files.push((rel, path));
        }
    }
    Ok(())
}

/// Sanitize a Radicle RID to be a valid directory name.
fn sanitize_rid(rid: &str) -> String {
    rid.replace(":", "_").replace("/", "_")
}
