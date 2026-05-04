use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use tracing::info;

/// Extract tag values from a Nostr event's tag list.
fn get_tags(tags: &Tags, key: &str) -> Vec<String> {
    tags.iter()
        .filter_map(|t| {
            if t.kind() == TagKind::Custom(key.into()) {
                t.content().map(|c| c.to_string())
            } else {
                None
            }
        })
        .collect()
}

/// Get first tag value for a key.
fn get_tag(tags: &Tags, key: &str) -> Option<String> {
    get_tags(tags, key).into_iter().next()
}

/// Parsed data from a kind 41000 (TASK) Nostr event.
#[derive(Debug, Clone)]
pub struct TaskEvent {
    pub event_id: EventId,
    pub pubkey: String,
    pub job_id: String,
    pub reward_amount: String,
    pub reward_token: String,
    pub timeout_hours: u64,
    pub agent_msig: String,
    pub escrow_contract: String,
    pub repo_rid: String,
    pub verifier_did: Option<String>,
    pub verifier_fee: Option<String>,
    pub score_threshold: Option<u8>,
    pub category: Option<String>,
    pub skills: Vec<String>,
    pub description: String,
    /// SHA-256 hash of verify/ directory contents (from Nostr tag or MANIFEST).
    pub verify_hash: Option<String>,
}

impl TaskEvent {
    pub fn parse(event: &Event) -> Option<Self> {
        if event.kind.as_u16() != 41000 {
            return None;
        }

        let reward_parts = get_tags(&event.tags, "reward");
        let timeout = get_tags(&event.tags, "timeout");

        Some(TaskEvent {
            event_id: event.id,
            pubkey: event.pubkey.to_hex(),
            job_id: get_tag(&event.tags, "job_id")?,
            reward_amount: reward_parts.first().cloned().unwrap_or_default(),
            reward_token: reward_parts.get(1).cloned().unwrap_or_else(|| "near".into()),
            timeout_hours: timeout.first().and_then(|t| t.parse().ok()).unwrap_or(24),
            agent_msig: get_tag(&event.tags, "agent")?,
            escrow_contract: get_tag(&event.tags, "escrow")?,
            repo_rid: get_tag(&event.tags, "repo_rid").unwrap_or_default(),
            verifier_did: get_tag(&event.tags, "verifier_did"),
            verifier_fee: get_tag(&event.tags, "verifier_fee"),
            score_threshold: get_tag(&event.tags, "score_threshold").and_then(|s| s.parse().ok()),
            category: get_tag(&event.tags, "category"),
            skills: get_tags(&event.tags, "skills"),
            description: event.content.clone(),
            verify_hash: get_tag(&event.tags, "verify_hash"),
        })
    }
}

/// Parsed data from a kind 41002 (WORKER_RESULT) Nostr event.
#[derive(Debug, Clone)]
pub struct WorkerResultEvent {
    pub event_id: EventId,
    pub pubkey: String,
    pub job_id: String,
    pub worker_msig: String,
    /// Radicle patch ID — references the worker's submitted patch (from `rad patch`).
    pub patch_id: String,
    pub branch: Option<String>,
    pub summary: String,
}

impl WorkerResultEvent {
    pub fn parse(event: &Event) -> Option<Self> {
        if event.kind.as_u16() != 41002 {
            return None;
        }

        Some(WorkerResultEvent {
            event_id: event.id,
            pubkey: event.pubkey.to_hex(),
            job_id: get_tag(&event.tags, "job_id")?,
            worker_msig: get_tag(&event.tags, "worker_msig")?,
            patch_id: get_tag(&event.tags, "patch_id").unwrap_or_default(),
            branch: get_tag(&event.tags, "branch"),
            summary: event.content.clone(),
        })
    }
}

/// Parsed data from a kind 41004 (FUNDED) Nostr event.
#[derive(Debug, Clone)]
pub struct FundedEvent {
    pub event_id: EventId,
    pub original_task_event_id: Option<EventId>,
    pub job_id: String,
}

impl FundedEvent {
    pub fn parse(event: &Event) -> Option<Self> {
        if event.kind.as_u16() != 41004 {
            return None;
        }

        Some(FundedEvent {
            event_id: event.id,
            original_task_event_id: event.tags.iter()
                .find(|t| t.kind() == TagKind::e())
                .and_then(|t| t.content())
                .and_then(|hex| EventId::from_hex(hex).ok()),
            job_id: get_tag(&event.tags, "job_id")?,
        })
    }
}

/// Nostr client that subscribes to escrow events and provides them via a channel.
pub struct NostrListener {
    client: Client,
}

impl NostrListener {
    pub async fn new(relay_urls: &[String], nostr_key_hex: &str) -> Result<Self> {
        let keys = Keys::parse(nostr_key_hex)
            .context("Invalid Nostr private key")?;

        let client = Client::new(keys);

        for url in relay_urls {
            info!("Connecting to Nostr relay: {}", url);
            client.add_relay(url).await?;
        }

        client.connect().await;
        Ok(Self { client })
    }

    /// Subscribe to all escrow-related events and yield them via a channel.
    pub async fn subscribe(
        &self,
    ) -> Result<(
        tokio::sync::mpsc::Receiver<TaskEvent>,
        tokio::sync::mpsc::Receiver<WorkerResultEvent>,
        tokio::sync::mpsc::Receiver<FundedEvent>,
    )> {
        let (task_tx, task_rx) = tokio::sync::mpsc::channel(256);
        let (result_tx, result_rx) = tokio::sync::mpsc::channel(256);
        let (funded_tx, funded_rx) = tokio::sync::mpsc::channel(256);

        let filter = Filter::new()
            .kinds(vec![Kind::Custom(41000), Kind::Custom(41002), Kind::Custom(41004)])
            .since(Timestamp::now());

        self.client.subscribe(vec![filter], None).await?;

        let client_clone = self.client.clone();
        tokio::spawn(async move {
            let mut notifications = client_clone.notifications();

            while let Ok(notification) = notifications.recv().await {
                if let RelayPoolNotification::Event { event, .. } = notification {
                    let kind = event.kind.as_u16();

                    if kind == 41000 {
                        if let Some(task) = TaskEvent::parse(&event) {
                            info!("TASK event: job_id={} repo_rid={}", task.job_id, task.repo_rid);
                            let _ = task_tx.send(task).await;
                        }
                    } else if kind == 41002 {
                        if let Some(result) = WorkerResultEvent::parse(&event) {
                            info!("WORKER_RESULT: job_id={} patch_id={}", result.job_id, result.patch_id);
                            let _ = result_tx.send(result).await;
                        }
                    } else if kind == 41004 {
                        if let Some(funded) = FundedEvent::parse(&event) {
                            info!("FUNDED: job_id={}", funded.job_id);
                            let _ = funded_tx.send(funded).await;
                        }
                    }
                }
            }
        });

        Ok((task_rx, result_rx, funded_rx))
    }

    /// Post a kind 41006 (VERIFIED) event to Nostr.
    pub async fn post_verified(
        &self,
        original_task_event_id: &EventId,
        job_id: &str,
        verifier_account: &str,
        verifier_did: &str,
        patch_id: &str,
        passed: bool,
        score: u8,
        method: &str,
        duration_ms: u64,
        detail: &str,
    ) -> Result<EventId> {
        let event = EventBuilder::new(
            Kind::Custom(41006),
            serde_json::json!({
                "passed": passed,
                "score": score,
                "method": method,
                "duration_ms": duration_ms,
                "detail": detail,
            }).to_string(),
        )
        .tag(Tag::event(*original_task_event_id))
        .tag(Tag::custom(
            TagKind::Custom("job_id".into()),
            [job_id],
        ))
        .tag(Tag::custom(
            TagKind::Custom("verifier".into()),
            [verifier_account],
        ))
        .tag(Tag::custom(
            TagKind::Custom("verifier_did".into()),
            [verifier_did],
        ))
        .tag(Tag::custom(
            TagKind::Custom("patch_id".into()),
            [patch_id],
        ))
        .tag(Tag::custom(
            TagKind::Custom("passed".into()),
            [if passed { "true" } else { "false" }],
        ))
        .tag(Tag::custom(
            TagKind::Custom("score".into()),
            [&score.to_string()],
        ))
        .tag(Tag::custom(
            TagKind::Custom("duration_ms".into()),
            [&duration_ms.to_string()],
        ))
        .tag(Tag::custom(
            TagKind::Custom("method".into()),
            [method],
        ));

        let output = self.client.send_event_builder(event).await?;
        info!("Posted VERIFIED event for job {}", job_id);
        Ok(*output)
    }
}
