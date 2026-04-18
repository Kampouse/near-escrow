use anyhow::Result;
use serde_json::json;
use ed25519_dalek::{Signer, SigningKey};
use tracing::{info, warn, error};
use crate::config::Config;
use crate::scorer::{Scorer, Verdict};
use crate::submitter;

/// Off-chain consensus via Nostr (kind 41006 for scores, 41007 for final verdict)
/// + on-chain submission via `resume_verification_multi`.
pub struct Consensus {
    config: Config,
    signing_key: SigningKey,
    scorer: Scorer,
}

impl Consensus {
    pub fn new(config: Config, signing_key: SigningKey) -> Self {
        let scorer = Scorer::new(config.clone());
        Self { config, signing_key, scorer }
    }

    /// Main loop: poll escrow for verifying escrows, score them, reach consensus, submit.
    pub async fn run(&self) -> Result<()> {
        info!("Starting consensus loop...");
        loop {
            if let Err(e) = self.tick().await {
                error!("Tick error: {:?}", e);
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
        }
    }

    async fn tick(&self) -> Result<()> {
        // 1. Find escrows in Verifying state
        let verifying = submitter::list_verifying(&self.config).await?;
        if verifying.is_empty() {
            return Ok(());
        }

        for escrow in verifying {
            let job_id = escrow["job_id"].as_str().unwrap_or("unknown");
            let data_id = escrow["data_id"].as_str().unwrap_or("");

            if data_id.is_empty() {
                continue;
            }

            info!("Processing job: {} (data_id: {})", job_id, data_id);

            // 2. Get task details
            let details = submitter::get_escrow(&self.config, job_id).await?;
            let task = details["task_description"].as_str().unwrap_or("");
            let criteria = details["criteria"].as_str().unwrap_or("");
            let result = details["result"].as_str().unwrap_or("");

            if result.is_empty() {
                warn!("No result yet for job {}", job_id);
                continue;
            }

            // 3. Score with LLM
            let verdict = self.scorer.score(task, criteria, result).await?;
            info!("Scored job {}: {}/100 — {}", job_id, verdict.score, verdict.detail);

            // 4. Sign and publish to Nostr (off-chain consensus)
            let verdict_json = json!({
                "score": verdict.score,
                "passed": verdict.passed,
                "detail": verdict.detail,
            }).to_string();

            let scoped_message = format!("{}:{}", data_id, verdict_json);
            let signature = self.signing_key.sign(scoped_message.as_bytes());

            // 5. Submit on-chain (single verifier mode for now)
            // In full production: wait for other verifiers' scores via Nostr first
            self.submit_verdict(data_id, &verdict_json, &signature.to_bytes()).await?;
        }

        Ok(())
    }

    async fn submit_verdict(&self, data_id: &str, verdict_json: &str, signature: &[u8]) -> Result<()> {
        submitter::resume_verification_multi(
            &self.config,
            data_id,
            verdict_json,
            self.config.verifier_index,
            signature,
        ).await
    }
}
