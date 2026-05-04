use anyhow::Result;
use ed25519_dalek::Signer;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{info, warn, error};

use crate::config::Config;
use crate::executor::{DockerExecutor, VerificationResult};
use crate::nostr::{NostrListener, TaskEvent, WorkerResultEvent, FundedEvent};
use crate::radicle::RadicleClient;
use crate::scorer::Scorer;
use crate::submitter;

/// The main verifier loop.
///
/// Watches Nostr for WORKER_RESULT events, clones the Radicle repo,
/// runs verify.sh in Docker, signs the verdict, submits on-chain,
/// and posts the result back to Nostr.
pub struct Verifier {
    config: Config,
    signing_key: ed25519_dalek::SigningKey,
    nostr: NostrListener,
    radicle: RadicleClient,
    executor: DockerExecutor,
    scorer: Scorer,
    /// Track which tasks we've already processed (job_id → patch_id)
    processed: Arc<Mutex<HashMap<String, String>>>,
    /// Map job_id → repo_rid (from TASK events)
    task_repos: Arc<Mutex<HashMap<String, String>>>,
    /// Map job_id → original task event ID (for VERIFIED event linking)
    task_event_ids: Arc<Mutex<HashMap<String, nostr_sdk::prelude::EventId>>>,
}

impl Verifier {
    pub async fn new(config: Config, signing_key: ed25519_dalek::SigningKey) -> Result<Self> {
        // Ensure work directory exists
        std::fs::create_dir_all(&config.work_dir)?;

        // Initialize Nostr client
        let nostr = NostrListener::new(&config.nostr_relays, &config.nostr_key_hex).await?;
        info!("Nostr client connected to {} relays", config.nostr_relays.len());

        // Initialize Radicle client
        let radicle = RadicleClient::new(&config.work_dir);
        info!("Radicle client ready, work_dir: {}", config.work_dir);

        // Initialize Docker executor
        let executor = DockerExecutor::new(
            &config.default_image,
            config.max_timeout_secs,
            config.max_memory_mb,
        );
        DockerExecutor::check_docker()?;

        // Initialize LLM scorer
        let scorer = Scorer::new(config.clone());

        Ok(Self {
            config,
            signing_key,
            nostr,
            radicle,
            executor,
            scorer,
            processed: Arc::new(Mutex::new(HashMap::new())),
            task_repos: Arc::new(Mutex::new(HashMap::new())),
            task_event_ids: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Main event loop. Subscribes to Nostr and processes events as they arrive.
    pub async fn run(&self) -> Result<()> {
        info!("Starting verifier event loop...");
        info!("  Verifier: {}", self.config.verifier_account_id);
        info!("  DID: {}", self.config.verifier_did);
        info!("  Escrow: {}", self.config.escrow_account);

        let (mut task_rx, mut result_rx, mut funded_rx) = self.nostr.subscribe().await?;

        loop {
            tokio::select! {
                Some(task) = task_rx.recv() => {
                    if let Err(e) = self.handle_task_event(task).await {
                        error!("Error handling TASK event: {:?}", e);
                    }
                }
                Some(result) = result_rx.recv() => {
                    if let Err(e) = self.handle_worker_result(result).await {
                        error!("Error handling WORKER_RESULT: {:?}", e);
                    }
                }
                Some(funded) = funded_rx.recv() => {
                    if let Err(e) = self.handle_funded(funded).await {
                        error!("Error handling FUNDED event: {:?}", e);
                    }
                }
            }
        }
    }

    /// Handle kind 41000 (TASK) — store repo_rid mapping for later use.
    async fn handle_task_event(&self, task: TaskEvent) -> Result<()> {
        let mut repos = self.task_repos.lock().await;
        let mut event_ids = self.task_event_ids.lock().await;

        repos.insert(task.job_id.clone(), task.repo_rid.clone());
        event_ids.insert(task.job_id.clone(), task.event_id);

        info!(
            "📝 Tracked task: {} → repo_rid={}",
            task.job_id, task.repo_rid
        );

        Ok(())
    }

    /// Handle kind 41004 (FUNDED) — confirm we know about the task, ready for work.
    async fn handle_funded(&self, funded: FundedEvent) -> Result<()> {
        let repos = self.task_repos.lock().await;
        if let Some(rid) = repos.get(&funded.job_id) {
            info!("💰 Task {} funded, repo_rid={}. Ready to verify.", funded.job_id, rid);
        } else {
            warn!("💰 Task {} funded but we don't have a TASK event for it yet", funded.job_id);
        }
        Ok(())
    }

    /// Handle kind 41002 (WORKER_RESULT) — the core verification pipeline.
    ///
    /// Fork+patch flow:
    ///   1. Worker forks the agent's repo, commits output/ to their fork
    ///   2. Worker submits a Radicle patch (rad patch), posts kind 41002 with patch_id
    ///   3. Verifier clones the agent's repo, checks out the worker's patch
    ///   4. Verifier runs agent's verify/ against worker's output/ in Docker sandbox
    ///
    /// The worker never has push access to the agent's repo, so verify/ is tamper-proof.
    async fn handle_worker_result(&self, result: WorkerResultEvent) -> Result<()> {
        let job_id = result.job_id.clone();
        let patch_id = result.patch_id.clone();

        // Dedup: skip if we already processed this job+patch
        {
            let processed = self.processed.lock().await;
            if let Some(prev) = processed.get(&job_id) {
                if *prev == patch_id {
                    info!("Skipping already-processed job {} patch {}", job_id, patch_id);
                    return Ok(());
                }
            }
        }

        info!("═══════════════════════════════════════════");
        info!("👷 Worker submitted result for job: {}", job_id);
        info!("   Patch ID: {}", patch_id);
        info!("   Worker msig: {}", result.worker_msig);
        info!("═══════════════════════════════════════════");

        // 1. Look up repo_rid for this job
        let repo_rid = {
            let repos = self.task_repos.lock().await;
            repos.get(&job_id).cloned()
        };

        let repo_rid = match repo_rid {
            Some(rid) => rid,
            None => {
                warn!("No repo_rid found for job {}. Falling back to contract data.", job_id);
                // Try to get escrow details from contract (task_repo_rid field)
                let escrow = match submitter::get_escrow(&self.config, &job_id).await {
                    Ok(e) => e,
                    Err(e) => {
                        error!("Failed to get escrow details: {:?}", e);
                        return Err(e);
                    }
                };
                match escrow.get("task_repo_rid").and_then(|v| v.as_str()) {
                    Some(rid) => rid.to_string(),
                    None => {
                        error!("No task_repo_rid on contract either. Cannot verify.");
                        return Ok(());
                    }
                }
            }
        };

        // 2. Clone the agent's Radicle repo (contains MANIFEST, input/, verify/)
        let repo_dir = match self.radicle.clone_repo(&repo_rid) {
            Ok(dir) => dir,
            Err(e) => {
                error!("Failed to clone repo {}: {:?}", repo_rid, e);
                return Ok(());
            }
        };

        // 3. Save the agent's base commit (main/master HEAD) for verify/ reference
        let base_commit = match self.radicle.base_commit(&repo_dir) {
            Ok(sha) => sha,
            Err(e) => {
                warn!("Could not determine base commit: {:?}. Using current HEAD.", e);
                self.radicle.head_commit(&repo_dir).unwrap_or_default()
            }
        };
        info!("Agent base commit: {}", base_commit);

        // 4. Checkout the worker's patch — creates a local branch with the worker's output
        let patch_commit = match self.radicle.checkout_patch(&repo_dir, &patch_id) {
            Ok(sha) => sha,
            Err(e) => {
                error!("Failed to checkout patch {}: {:?}", patch_id, e);
                // Fallback: try branch checkout if patch_id looks like a commit SHA
                if let Some(ref branch) = result.branch {
                    if let Err(e2) = self.radicle.checkout_branch(&repo_dir, branch) {
                        error!("Failed to checkout branch {} either: {:?}", branch, e2);
                        return Ok(());
                    }
                } else {
                    return Ok(());
                }
                String::new()
            }
        };

        // 5. Read MANIFEST.json from the agent's base commit (not the worker's patch!)
        let manifest: Option<serde_json::Value> = self.radicle.read_file_at_commit(
            &repo_dir, &base_commit, "MANIFEST.json"
        ).ok()
            .and_then(|c| serde_json::from_str(&c).ok());

        // 6. Check if verify.sh exists in the agent's base commit
        let has_verify_script = self.radicle.read_file_at_commit(
            &repo_dir, &base_commit, "verify/verify.sh"
        ).is_ok();

        let verify_method = manifest
            .as_ref()
            .and_then(|m| m.get("verification"))
            .and_then(|v| v.get("method"))
            .and_then(|m| m.as_str())
            .unwrap_or("test_suite");

        // The patch checkout dir contains the worker's output/ — we pass it
        // to the executor so it mounts verify/ from base and output/ from patch.
        let patch_dir = repo_dir.as_path();

        let verification_result = match verify_method {
            "test_suite" | "deterministic" if has_verify_script => {
                // First, checkout base commit to get clean verify/ for docker mount
                self.radicle.checkout_commit(&repo_dir, &base_commit)?;
                let runtime_image = manifest
                    .as_ref()
                    .and_then(|m| m.get("execution"))
                    .and_then(|e| e.get("runtime"))
                    .and_then(|r| r.as_str())
                    .unwrap_or(&self.config.default_image);

                self.executor.verify_with_image(&repo_dir, runtime_image, Some(patch_dir))?
            }
            "llm_judge" => {
                // Fall back to LLM scoring (no verify.sh needed)
                let task_desc = manifest
                    .as_ref()
                    .and_then(|m| m.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or("Task");

                let criteria = manifest
                    .as_ref()
                    .and_then(|m| m.get("verification"))
                    .and_then(|v| v.get("criteria"))
                    .and_then(|c| c.as_str())
                    .unwrap_or("Quality of work");

                // Read worker output from the patch checkout
                let output_text = self.radicle.read_file(&repo_dir, "output/result.json")
                    .unwrap_or_else(|_| self.radicle.read_file(&repo_dir, "output/solution.py")
                    .unwrap_or_else(|_| result.summary.clone()));

                let verdict = self.scorer.score(task_desc, criteria, &output_text).await?;
                VerificationResult {
                    passed: verdict.passed,
                    exit_code: if verdict.passed { 0 } else { 1 },
                    stdout: verdict.detail.clone(),
                    stderr: String::new(),
                    duration_ms: 0,
                    method: "llm_judge".into(),
                }
            }
            _ => {
                warn!("No verification method available for job {}", job_id);
                return Ok(());
            }
        };

        // 7. Compute score
        let score = if verification_result.passed {
            manifest
                .as_ref()
                .and_then(|m| m.get("verification"))
                .and_then(|v| v.get("threshold"))
                .and_then(|t| t.as_u64())
                .map(|t| t as u8)
                .unwrap_or(100) // If passed, default to 100
        } else {
            0
        };

        info!("📊 Verification result: passed={} score={} method={}",
            verification_result.passed, score, verification_result.method);

        // 8. Sign the verdict
        let verdict_json = serde_json::json!({
            "score": score,
            "passed": verification_result.passed,
            "detail": &verification_result.stdout[..verification_result.stdout.len().min(1000)],
            "method": verification_result.method,
            "patch_id": patch_id,
        }).to_string();

        let scoped_message = format!("{}:{}", job_id, verdict_json);
        let signature = self.signing_key.sign(scoped_message.as_bytes());

        // 9. Submit on-chain
        let escrow = match submitter::get_escrow(&self.config, &job_id).await {
            Ok(e) => e,
            Err(e) => {
                error!("Failed to get escrow for on-chain submit: {:?}", e);
                return Ok(());
            }
        };

        let data_id = escrow.get("data_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if !data_id.is_empty() {
            match submitter::resume_verification_multi(
                &self.config,
                data_id,
                &verdict_json,
                self.config.verifier_index,
                &signature.to_bytes(),
            ).await {
                Ok(()) => info!("✅ Verdict submitted on-chain for job {}", job_id),
                Err(e) => error!("❌ Failed to submit verdict on-chain: {:?}", e),
            }
        } else {
            warn!("No data_id for job {}, skipping on-chain submission", job_id);
        }

        // 10. Post VERIFIED event to Nostr
        let original_event_id = {
            let event_ids = self.task_event_ids.lock().await;
            event_ids.get(&job_id).copied()
        };

        if let Some(event_id) = original_event_id {
            if let Err(e) = self.nostr.post_verified(
                &event_id,
                &job_id,
                &self.config.verifier_account_id,
                &self.config.verifier_did,
                &patch_id,
                verification_result.passed,
                score,
                &verification_result.method,
                verification_result.duration_ms,
                &verification_result.stdout[..verification_result.stdout.len().min(500)],
            ).await {
                error!("Failed to post VERIFIED to Nostr: {:?}", e);
            }
        }

        // 11. Mark as processed
        {
            let mut processed = self.processed.lock().await;
            processed.insert(job_id.clone(), patch_id.clone());
        }

        info!("═══════════════════════════════════════════");
        info!("Done with job {}", job_id);
        info!("═══════════════════════════════════════════");

        Ok(())
    }
}
