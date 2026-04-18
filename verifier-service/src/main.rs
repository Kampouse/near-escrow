/// escrow-verifier — Off-chain multi-verifier consensus service
///
/// Watches the escrow contract for `result_submitted` events (via Nostr or polling),
/// scores the work using an LLM, signs the verdict, and publishes it.
/// When enough verifiers agree (≥threshold), any verifier submits on-chain.
///
/// Usage:
///   escrow-verifier --config verifier.toml
///
/// verifier.toml:
///   verifier_index = 0          # Your index in the verifier_set
///   secret_key_hex = "..."      # ed25519 signing key (32 bytes hex)
///   escrow_account = "escrow.near"
///   network = "testnet"
///   rpc_url = "https://test.rpc.fastnear.com"
///   nostr_relay = "wss://nostr-relay-production.up.railway.app"
///   llm_url = "https://api.openai.com/v1/chat/completions"
///   llm_api_key = "sk-..."
///   llm_model = "gpt-4o-mini"

use anyhow::Result;
use clap::Parser;
use ed25519_dalek::{Signer, SigningKey};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tracing::{info, warn, error};

mod config;
mod scorer;
mod consensus;
mod submitter;

#[derive(Parser)]
#[command(name = "escrow-verifier", about = "Multi-verifier consensus service for NEAR escrow")]
struct Cli {
    #[arg(short, long, default_value = "verifier.toml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("escrow_verifier=info")
        .init();

    let cli = Cli::parse();
    let config = config::load(&cli.config)?;

    info!("Starting escrow-verifier");
    info!("  Verifier index: {}", config.verifier_index);
    info!("  Escrow: {}", config.escrow_account);
    info!("  Network: {}", config.network);
    info!("  Consensus threshold: {}", config.consensus_threshold);

    let signing_key_bytes: [u8; 32] = hex::decode(&config.secret_key_hex)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("secret_key_hex must be 32 bytes"))?;
    let signing_key = SigningKey::from_bytes(&signing_key_bytes);
    let public_key = signing_key.verifying_key();
    info!("  Public key: {}", hex::encode(public_key.as_bytes()));

    // Verify our key matches what's registered on-chain
    let verifier_set = submitter::get_verifier_set(&config).await?;
    let our_info = verifier_set.get(config.verifier_index as usize)
        .ok_or_else(|| anyhow::anyhow!("Verifier index {} out of bounds (set has {} verifiers)",
            config.verifier_index, verifier_set.len()))?;

    if hex::encode(public_key.as_bytes()) != our_info.public_key {
        anyhow::bail!("Our public key doesn't match on-chain verifier_set[{}]. Expected: {}, Got: {}",
            config.verifier_index, our_info.public_key, hex::encode(public_key.as_bytes()));
    }
    info!("✅ Key matches on-chain verifier_set[{}]", config.verifier_index);

    // Start the consensus loop
    let consensus = consensus::Consensus::new(config, signing_key);
    consensus.run().await?;

    Ok(())
}
