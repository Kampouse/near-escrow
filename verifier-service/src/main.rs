/// escrow-verifier — Nostr-native verifier service for NEAR escrow
///
/// Three-layer architecture:
///   Nostr     = discovery, coordination, signed action relay
///   Radicle   = git hosting, task delivery, access control
///   NEAR      = payment, escrow, settlement
///
/// Watches Nostr for WORKER_RESULT events, clones the Radicle task repo,
/// runs verify.sh in a Docker container, signs the verdict, submits on-chain,
/// and publishes the result back to Nostr.
///
/// Usage:
///   escrow-verifier --config verifier.toml

use anyhow::Result;
use clap::Parser;
use ed25519_dalek::Signer;
use std::path::PathBuf;
use tracing::info;

mod config;
mod consensus;
mod executor;
mod nostr;
mod radicle;
mod scorer;
mod submitter;

#[derive(Parser)]
#[command(name = "escrow-verifier", about = "Nostr-native verifier service for NEAR escrow")]
struct Cli {
    #[arg(short, long, default_value = "verifier.toml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "escrow_verifier=info".into()),
        )
        .init();

    let cli = Cli::parse();
    let config = config::load(&cli.config)?;

    info!("╔══════════════════════════════════════╗");
    info!("║     escrow-verifier starting        ║");
    info!("╠══════════════════════════════════════╣");
    info!("║ Verifier:  {}               ║", config.verifier_account_id);
    info!("║ DID:       {}...  ║", &config.verifier_did[..config.verifier_did.len().min(20)]);
    info!("║ Escrow:    {}          ║", config.escrow_account);
    info!("║ Network:   {:20}     ║", config.network);
    info!("║ Relays:    {}                 ║", config.nostr_relays.len());
    info!("╚══════════════════════════════════════╝");

    // Build signing key
    let signing_key_bytes: [u8; 32] = hex::decode(&config.secret_key_hex)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("secret_key_hex must be 32 bytes"))?;
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&signing_key_bytes);
    let public_key = signing_key.verifying_key();
    info!("Public key: {}", hex::encode(public_key.as_bytes()));

    // Verify our key matches on-chain (if contract is deployed)
    match submitter::get_verifier_set(&config).await {
        Ok(verifier_set) => {
            let our_info = verifier_set.get(config.verifier_index as usize);
            match our_info {
                Some(info) => {
                    if hex::encode(public_key.as_bytes()) != info.public_key {
                        anyhow::bail!(
                            "Our public key doesn't match on-chain verifier_set[{}]. Expected: {}, Got: {}",
                            config.verifier_index, info.public_key, hex::encode(public_key.as_bytes())
                        );
                    }
                    info!("✅ Key matches on-chain verifier_set[{}]", config.verifier_index);
                }
                None => {
                    tracing::warn!("Verifier index {} out of bounds (set has {} verifiers). Running without on-chain check.",
                        config.verifier_index, verifier_set.len());
                }
            }
        }
        Err(e) => {
            tracing::warn!("Could not verify on-chain verifier_set (contract may not be deployed yet): {:?}", e);
        }
    }

    // Start the main verifier loop
    let verifier = consensus::Verifier::new(config, signing_key).await?;
    verifier.run().await
}
