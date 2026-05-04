use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Config {
    // ─── Identity ────────────────────────────────────────────
    /// Our index in the escrow's verifier_set
    pub verifier_index: u8,
    /// ed25519 secret key (32 bytes hex) — MUST match the public key in verifier_set
    pub secret_key_hex: String,
    /// Verifier's NEAR account ID (used to sign transactions)
    pub verifier_account_id: String,
    /// Verifier's Radicle DID (posted in Nostr events for discovery)
    pub verifier_did: String,

    // ─── Contracts ───────────────────────────────────────────
    /// Escrow contract account ID
    pub escrow_account: String,
    /// "testnet" or "mainnet"
    pub network: String,
    /// NEAR RPC URL
    pub rpc_url: String,

    // ─── Nostr ───────────────────────────────────────────────
    /// Nostr relay URLs
    #[serde(default = "default_nostr_relays")]
    pub nostr_relays: Vec<String>,
    /// Nostr secp256k1 private key (hex) for event signing
    pub nostr_key_hex: String,

    // ─── Radicle ────────────────────────────────────────────
    /// Radicle node home directory
    #[serde(default = "default_radicle_home")]
    pub radicle_home: String,
    /// Directory for cloned task repos
    #[serde(default = "default_work_dir")]
    pub work_dir: String,
    /// Act as a Radicle seed node (always-on)
    #[serde(default)]
    pub radicle_seed: bool,

    // ─── Executor ────────────────────────────────────────────
    /// Docker runtime: "docker" or "podman"
    #[serde(default = "default_runtime")]
    pub executor_runtime: String,
    /// Default container image for verification
    #[serde(default = "default_image")]
    pub default_image: String,
    /// Max verification timeout (seconds)
    #[serde(default = "default_timeout")]
    pub max_timeout_secs: u64,
    /// Max container memory (MB)
    #[serde(default = "default_memory")]
    pub max_memory_mb: u64,

    // ─── LLM Judge ──────────────────────────────────────────
    /// LLM API URL (for llm_judge verification method)
    #[serde(default = "default_llm_url")]
    pub llm_url: String,
    /// LLM API key
    #[serde(default)]
    pub llm_api_key: String,
    /// LLM model name
    #[serde(default = "default_llm_model")]
    pub llm_model: String,

    // ─── Scoring ─────────────────────────────────────────────
    /// Consensus threshold (must match escrow contract)
    #[serde(default = "default_threshold")]
    pub consensus_threshold: u8,
    /// Score threshold — scores below this are "failed" (must match escrow)
    #[serde(default = "default_score_threshold")]
    pub score_threshold: u8,
}

fn default_threshold() -> u8 { 2 }
fn default_score_threshold() -> u8 { 50 }
fn default_nostr_relays() -> Vec<String> {
    vec!["wss://nostr-relay-production.up.railway.app".into()]
}
fn default_radicle_home() -> String {
    dirs_home().join(".radicle").to_string_lossy().into_owned()
}
fn default_work_dir() -> String {
    dirs_home().join(".hermes/verifier/repos").to_string_lossy().into_owned()
}
fn default_runtime() -> String { "docker".into() }
fn default_image() -> String { "python:3.12-slim".into() }
fn default_timeout() -> u64 { 600 }
fn default_memory() -> u64 { 2048 }
fn default_llm_url() -> String { "https://api.openai.com/v1/chat/completions".into() }
fn default_llm_model() -> String { "gpt-4o-mini".into() }

fn dirs_home() -> std::path::PathBuf {
    std::env::var("HOME").map(|h| std::path::PathBuf::from(h)).unwrap_or_else(|_| "/tmp".into())
}

#[derive(Debug, Deserialize, Clone)]
pub struct VerifierInfo {
    pub account_id: String,
    pub public_key: String,
    pub active: bool,
}

pub fn load(path: &Path) -> Result<Config> {
    let content = fs::read_to_string(path)?;
    let config: Config = toml::from_str(&content)?;
    Ok(config)
}
