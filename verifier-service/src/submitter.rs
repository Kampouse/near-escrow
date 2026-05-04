use anyhow::{bail, Context, Result};
use ed25519_dalek::Signer;
use serde_json::json;
use tracing::{info, warn, error};
use crate::config::{Config, VerifierInfo};

/// Submit signed verdict to the escrow contract on-chain.
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

    send_transaction(
        config,
        "resume_verification_multi",
        serde_json::to_vec(&args)?,
        200, // 200 Tgas
        0,   // no deposit
    ).await
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

// ─── RPC helpers (raw JSONRPC via reqwest) ────────────────────────────────────

/// Send a signed transaction on-chain.
async fn send_transaction(
    config: &Config,
    method: &str,
    args: Vec<u8>,
    gas_tgas: u64,
    deposit_yocto: u128,
) -> Result<()> {
    let signer_account_id = &config.verifier_account_id;
    let escrow_account = &config.escrow_account;

    // Build ed25519-dalek signing key
    let secret_bytes = hex::decode(config.secret_key_hex.strip_prefix("0x").unwrap_or(&config.secret_key_hex))?;
    if secret_bytes.len() != 32 {
        bail!("Secret key must be 32 bytes");
    }
    let mut key_bytes = [0u8; 32];
    key_bytes.copy_from_slice(&secret_bytes);
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&key_bytes);
    let verifying_key = signing_key.verifying_key();
    let public_key_bytes = verifying_key.as_bytes();

    // NEAR public key format: "ed25519:<base64>"
    let public_key_str = format!("ed25519:{}", base64_encode(public_key_bytes.to_vec()));

    // 1. Get nonce via query
    let access_key_resp = rpc_call(config, "query", json!({
        "request_type": "view_access_key",
        "finality": "final",
        "account_id": signer_account_id,
        "public_key": public_key_str,
    })).await?;

    let nonce = access_key_resp["result"]["nonce"]
        .as_u64()
        .context("No nonce in access key response")? + 1;

    // 2. Get block hash
    let block_resp = rpc_call(config, "block", json!({
        "finality": "final",
    })).await?;

    let block_hash_hex = block_resp["header"]["hash"]
        .as_str()
        .context("No block hash")?;
    let block_hash = hex::decode(block_hash_hex.strip_prefix("0x").unwrap_or(block_hash_hex))?;
    let block_hash_b58 = bs58_encode(&block_hash);

    // 3. Build transaction
    let args_b64 = base64_encode(args);

    let tx = json!({
        "signer_id": signer_account_id,
        "public_key": public_key_str,
        "nonce": nonce,
        "receiver_id": escrow_account,
        "block_hash": block_hash_b58,
        "actions": [{
            "FunctionCall": {
                "method_name": method,
                "args": args_b64,
                "gas": gas_tgas * 1_000_000_000_000u64,
                "deposit": deposit_yocto.to_string(),
            }
        }]
    });

    // 4. Serialize and sign
    let tx_bytes = serde_json::to_vec(&tx)?;
    let signature = signing_key.sign(&tx_bytes);
    let sig_b64 = base64_encode(signature.to_bytes().to_vec());

    let signed_tx = json!({
        "transaction": tx,
        "signature": format!("ed25519:{}", sig_b64),
    });

    info!("Sending tx: {} (nonce: {})", method, nonce);

    // 5. Broadcast
    let send_resp = rpc_call(config, "broadcast_tx_commit", json!([signed_tx])).await?;

    if let Some(err) = send_resp.get("error") {
        error!("❌ TX error: {:?}", err);
        bail!("Transaction error: {:?}", err);
    }

    let status = send_resp["status"]["SuccessValue"].as_str();
    if status.is_some() || send_resp["transaction_outcome"].is_object() {
        info!("✅ TX succeeded: {}", method);
        Ok(())
    } else {
        error!("❌ TX may have failed: {:?}", send_resp);
        bail!("Transaction outcome unclear: {:?}", send_resp);
    }
}

/// Call a view function on the escrow contract.
async fn view_function(config: &Config, method: &str, args: &serde_json::Value) -> Result<serde_json::Value> {
    let args_b64 = base64_encode(serde_json::to_vec(args)?);

    let resp = rpc_call(config, "query", json!({
        "request_type": "call_function",
        "finality": "final",
        "account_id": config.escrow_account,
        "method_name": method,
        "args_base64": args_b64,
    })).await?;

    if let Some(err) = resp.get("error") {
        bail!("View call {} error: {:?}", method, err);
    }

    let result_array = resp["result"]["result"]
        .as_array()
        .context("No result from view call")?;

    let bytes: Vec<u8> = result_array.iter()
        .filter_map(|v| v.as_u64().map(|n| n as u8))
        .collect();

    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    Ok(value)
}

/// Raw JSONRPC call.
async fn rpc_call(config: &Config, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
    let client = reqwest::Client::new();
    let body = json!({
        "jsonrpc": "2.0",
        "id": format!("v-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis()),
        "method": method,
        "params": params,
    });

    let resp: serde_json::Value = client
        .post(&config.rpc_url)
        .json(&body)
        .send()
        .await?
        .json()
        .await?;

    Ok(resp)
}

fn base64_encode(data: Vec<u8>) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(&data)
}

fn bs58_encode(data: &[u8]) -> String {
    // Simple bs58 encoding for block hash (32 bytes → base58)
    const ALPHABET: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
    let mut result = Vec::new();
    let mut num = vec![0u8; data.len()];

    // Count leading zeros
    let mut leading_zeros = 0;
    for &b in data {
        if b == 0 { leading_zeros += 1; } else { break; }
    }

    // Convert to base58
    let mut bytes = data.to_vec();
    while !bytes.iter().all(|&b| b == 0) {
        let mut carry = 0u32;
        for byte in bytes.iter_mut() {
            let val = (carry << 8) | (*byte as u32);
            *byte = (val / 58) as u8;
            carry = val % 58;
        }
        result.push(ALPHABET[carry as usize]);
    }

    // Add leading '1's for leading zeros
    for _ in 0..leading_zeros {
        result.push(b'1');
    }

    result.reverse();
    String::from_utf8(result).unwrap_or_default()
}
