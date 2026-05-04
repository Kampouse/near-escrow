use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Command;
use std::time::Instant;
use tracing::{info, warn, error};

/// Result of running verification.
#[derive(Debug, Serialize, Deserialize)]
pub struct VerificationResult {
    pub passed: bool,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
    pub method: String,
}

/// Reads MANIFEST.json from a repo and returns the verification method.
fn read_manifest(repo_dir: &Path) -> Result<serde_json::Value> {
    let manifest_path = repo_dir.join("MANIFEST.json");
    let content = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("Failed to read {}", manifest_path.display()))?;
    serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse MANIFEST.json"))
}

/// Run verification in a Docker container.
pub struct DockerExecutor {
    default_image: String,
    default_timeout_secs: u64,
    default_memory_mb: u64,
}

impl DockerExecutor {
    pub fn new(default_image: &str, default_timeout_secs: u64, default_memory_mb: u64) -> Self {
        Self {
            default_image: default_image.to_string(),
            default_timeout_secs,
            default_memory_mb,
        }
    }

    /// Run verify.sh in a sandboxed Docker container.
    /// Single mount: the entire checked-out repo (verify/ integrity verified via hash before this call).
    pub fn verify(&self, repo_dir: &Path) -> Result<VerificationResult> {
        // Read manifest for execution config
        let manifest = read_manifest(repo_dir).ok();
        let verify_config = manifest
            .as_ref()
            .and_then(|m| m.get("verification").cloned())
            .unwrap_or_default();

        let method = verify_config.get("method")
            .and_then(|m| m.as_str())
            .unwrap_or("test_suite")
            .to_string();

        let timeout_secs = verify_config.get("timeout_seconds")
            .and_then(|t| t.as_u64())
            .unwrap_or(self.default_timeout_secs);

        let memory_mb = verify_config.get("memory_mb")
            .and_then(|m| m.as_u64())
            .unwrap_or(self.default_memory_mb);

        let verify_script = repo_dir.join("verify").join("verify.sh");

        if !verify_script.exists() {
            return Ok(VerificationResult {
                passed: false,
                exit_code: -1,
                stdout: String::new(),
                stderr: "verify/verify.sh not found in repo".into(),
                duration_ms: 0,
                method,
            });
        }

        info!(
            "Running verification: method={} timeout={}s memory={}MB",
            method, timeout_secs, memory_mb
        );

        let start = Instant::now();

        // Single mount: the whole repo at /task (verify/ integrity already checked via hash)
        let docker_args = vec![
            "run".to_string(),
            "--rm".to_string(),
            "--network".to_string(), "none".to_string(),
            "--memory".to_string(), format!("{}m", memory_mb),
            "--cpus".to_string(), "2".to_string(),
            "--read-only".to_string(),
            "--tmpfs".to_string(), "/tmp:size=100m".to_string(),
            "-v".to_string(), format!("{}:/task:ro", repo_dir.display()),
            "-w".to_string(), "/task".to_string(),
            self.default_image.clone(),
            "bash".to_string(), "verify/verify.sh".to_string(),
        ];

        let output = Command::new("docker")
            .args(&docker_args)
            .output()
            .context("Failed to run docker container")?;

        let duration_ms = start.elapsed().as_millis() as u64;
        let exit_code = output.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let passed = exit_code == 0;

        if passed {
            info!("✅ Verification passed ({}ms)", duration_ms);
        } else {
            warn!("❌ Verification failed (exit={}, {}ms)", exit_code, duration_ms);
            error!("  stderr: {}", &stderr[..stderr.len().min(500)]);
        }

        Ok(VerificationResult {
            passed,
            exit_code,
            stdout,
            stderr,
            duration_ms,
            method,
        })
    }

    /// Run verification with a specific container image (from MANIFEST.json execution.runtime).
    /// Single mount: the entire checked-out repo.
    pub fn verify_with_image(&self, repo_dir: &Path, image: &str) -> Result<VerificationResult> {
        let verify_script = repo_dir.join("verify").join("verify.sh");

        if !verify_script.exists() {
            return Ok(VerificationResult {
                passed: false,
                exit_code: -1,
                stdout: String::new(),
                stderr: "verify/verify.sh not found in repo".into(),
                duration_ms: 0,
                method: "unknown".into(),
            });
        }

        info!("Running verification with image: {}", image);

        let start = Instant::now();

        // Single mount: the whole repo at /task (verify/ integrity already checked via hash)
        let docker_args = vec![
            "run".to_string(),
            "--rm".to_string(),
            "--network".to_string(), "none".to_string(),
            "--memory".to_string(), format!("{}m", self.default_memory_mb),
            "--cpus".to_string(), "2".to_string(),
            "--read-only".to_string(),
            "--tmpfs".to_string(), "/tmp:size=100m".to_string(),
            "-v".to_string(), format!("{}:/task:ro", repo_dir.display()),
            "-w".to_string(), "/task".to_string(),
            image.to_string(),
            "bash".to_string(), "verify/verify.sh".to_string(),
        ];

        let output = Command::new("docker")
            .args(&docker_args)
            .output()
            .context("Failed to run docker container")?;

        let duration_ms = start.elapsed().as_millis() as u64;
        let exit_code = output.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let passed = exit_code == 0;

        Ok(VerificationResult {
            passed,
            exit_code,
            stdout,
            stderr,
            duration_ms,
            method: "container".into(),
        })
    }

    /// Check if Docker is available.
    pub fn check_docker() -> Result<()> {
        let output = Command::new("docker")
            .args(["--version"])
            .output()
            .context("Docker not found. Install Docker or Podman.")?;

        if !output.status.success() {
            anyhow::bail!("Docker check failed: {}", String::from_utf8_lossy(&output.stderr));
        }

        info!("Docker: {}", String::from_utf8_lossy(&output.stdout).trim());
        Ok(())
    }
}
