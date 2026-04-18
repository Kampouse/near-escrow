use anyhow::Result;
use serde_json::json;
use tracing::{info, warn};
use crate::config::{Config, VerifierInfo};

/// Submit signed verdict to the escrow contract via RPC.
pub async fn resume_verification_multi(
    config: &Config,
    data_id_hex: &str,
    verdict_json: &str,
    verifier_index: u8,
    signature: &[u8],
) -> Result<()> {
    let args = json!({
        "data_id_hex": data_id_hex,
        "signed_verdict": {
            "verdict_json": verdict_json,
            "signatures": [{
                "verifier_index": verifier_index,
                "signature": signature.to_vec(),
            }]
        }
    });

    info!("Submitting resume_verification_multi for data_id={}", data_id_hex);

    // For single-verifier mode (threshold=1), one sig is enough.
    // For multi-verifier mode (threshold=2+), need to collect sigs from Nostr first.

    call_function(config, "resume_verification_multi", &args).await
}

/// Get the verifier set from the escrow contract.
pub async fn get_verifier_set(config: &Config) -> Result<Vec<VerifierInfo>> {
    let result = view_function(config, "get_verifier_set", &json!({})).await?;
    let set: Vec<VerifierInfo> = serde_json::from_value(result)?;
    Ok(set)
}

/// Get escrows in Verifying state.
pub async fn list_verifying(config: &Config) -> Result<Vec<serde_json::Value>> {
    let result = view_function(config, "list_verifying", &json!({})).await?;
    let list: Vec<serde_json::Value> = serde_json::from_value(result)?;
    Ok(list)
}

/// Get escrow details.
pub async fn get_escrow(config: &Config, job_id: &str) -> Result<serde_json::Value> {
    view_function(config, "get_escrow", &json!({"job_id": job_id})).await
}

// ---- RPC helpers ----

async fn view_function(config: &Config, method: &str, args: &serde_json::Value) -> Result<serde_json::Value> {
    let client = reqwest::Client::new();
    let args_base64 = base64_encode(serde_json::to_vec(args)?);

    let response: serde_json::Value = client.post(&config.rpc_url)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "query",
            "params": {
                "request_type": "call_function",
                "finality": "final",
                "account_id": config.escrow_account,
                "method_name": method,
                "args_base64": args_base64,
            }
        }))
        .send().await?
        .json().await?;

    // Parse the result
    let result = response["result"]["result"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("No result from RPC: {:?}", response))?;

    let bytes: Vec<u8> = result.iter()
        .filter_map(|v| v.as_u64().map(|n| n as u8))
        .collect();

    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    Ok(value)
}

async fn call_function(config: &Config, method: &str, args: &serde_json::Value) -> Result<()> {
    // For now, log the transaction that needs to be signed and submitted.
    // Full implementation would use near-jsonrpc-client to send a signed transaction.
    info!(
        "TX: {}.{}({}) on {}",
        config.escrow_account, method,
        serde_json::to_string(args)?,
        config.network
    );
    warn!("On-chain submission not yet implemented — needs signer key for transaction signing");
    Ok(())
}

fn base64_encode(data: Vec<u8>) -> String {
    use std::fmt::Write;
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::new();
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let triple = (b0 << 16) | (b1 << 8) | b2;
        result.write_char(CHARS[((triple >> 18) & 0x3F) as usize] as char).unwrap();
        result.write_char(CHARS[((triple >> 12) & 0x3F) as usize] as char).unwrap();
        if chunk.len() > 1 {
            result.write_char(CHARS[((triple >> 6) & 0x3F) as usize] as char).unwrap();
        } else {
            result.push('=');
        }
        if chunk.len() > 2 {
            result.write_char(CHARS[(triple & 0x3F) as usize] as char).unwrap();
        } else {
            result.push('=');
        }
    }
    result
}
