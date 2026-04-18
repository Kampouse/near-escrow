use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Config {
    /// Our index in the escrow's verifier_set
    pub verifier_index: u8,
    /// ed25519 secret key (32 bytes hex) — MUST match the public key in verifier_set
    pub secret_key_hex: String,
    /// Escrow contract account ID
    pub escrow_account: String,
    /// "testnet" or "mainnet"
    pub network: String,
    /// NEAR RPC URL
    pub rpc_url: String,
    /// Nostr relay for off-chain consensus
    pub nostr_relay: String,
    /// LLM API URL
    pub llm_url: String,
    /// LLM API key
    pub llm_api_key: String,
    /// LLM model name
    pub llm_model: String,
    /// Consensus threshold (must match escrow contract)
    #[serde(default = "default_threshold")]
    pub consensus_threshold: u8,
    /// Score threshold — scores below this are "failed" (must match escrow)
    #[serde(default = "default_score_threshold")]
    pub score_threshold: u8,
}

fn default_threshold() -> u8 { 2 }
fn default_score_threshold() -> u8 { 50 }

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
