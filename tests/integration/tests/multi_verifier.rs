use ed25519_dalek::{Signer, SigningKey};
use near_workspaces::types::Gas as WsGas;
use serde_json::json;

const ESCROW_WASM: &str = "../../target/wasm32-unknown-unknown/release/near_escrow.wasm";
const FT_MOCK_WASM: &str = "../../target/wasm32-unknown-unknown/release/ft_mock.wasm";

const STORAGE_DEPOSIT: u128 = 1_000_000_000_000_000_000_000_000;
const WORKER_STAKE: u128 = 100_000_000_000_000_000_000_000;
const GAS_INIT: WsGas = WsGas::from_tgas(30);
const GAS_STORAGE: WsGas = WsGas::from_tgas(30);
const GAS_MINT: WsGas = WsGas::from_tgas(30);
const GAS_CLAIM: WsGas = WsGas::from_tgas(50);
const GAS_SUBMIT: WsGas = WsGas::from_tgas(300);
const GAS_RESUME: WsGas = WsGas::from_tgas(300);

fn gen_keypair_from(seed: u8) -> SigningKey {
    let mut bytes = [0u8; 32];
    bytes[0] = seed;
    SigningKey::from_bytes(&bytes)
}

fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn make_sig(sk: &SigningKey, data_id_hex: &str, verdict_json: &str, idx: u8) -> serde_json::Value {
    let scoped = format!("{}:{}", data_id_hex, verdict_json);
    json!({ "verifier_index": idx, "signature": sk.sign(scoped.as_bytes()).to_bytes().to_vec() })
}

fn build_signed(data_id_hex: &str, verdict_json: &str, keys: &[(usize, &SigningKey)]) -> serde_json::Value {
    let sigs: Vec<_> = keys.iter().map(|(i, sk)| make_sig(sk, data_id_hex, verdict_json, *i as u8)).collect();
    json!({ "verdict_json": verdict_json, "signatures": sigs })
}

struct TestEnv {
    worker: near_workspaces::Worker<near_workspaces::network::Sandbox>,
    escrow: near_workspaces::Contract,
    ft: near_workspaces::Contract,
    agent: near_workspaces::Account,
    task_worker: near_workspaces::Account,
    ska: SigningKey,
    skb: SigningKey,
    skc: SigningKey,
}

async fn setup() -> anyhow::Result<TestEnv> {
    let worker = near_workspaces::sandbox().await?;
    let escrow = worker.dev_deploy(&std::fs::read(ESCROW_WASM)?).await?;
    let ft = worker.dev_deploy(&std::fs::read(FT_MOCK_WASM)?).await?;
    let agent = worker.dev_create_account().await?;
    let task_worker = worker.dev_create_account().await?;

    let ska = gen_keypair_from(1);
    let skb = gen_keypair_from(2);
    let skc = gen_keypair_from(3);

    // Init FT
    ft.call("new").gas(GAS_INIT).transact().await?.into_result()?;
    ft.call("mint").args_json(json!({"account_id": agent.id(), "amount": "1000000000000000000000000"}))
        .gas(GAS_MINT).transact().await?.into_result()?;
    for aid in [escrow.id(), agent.id(), task_worker.id()] {
        ft.call("storage_deposit").args_json(json!({"account_id": aid}))
            .deposit(near_workspaces::types::NearToken::from_yoctonear(STORAGE_DEPOSIT))
            .gas(GAS_STORAGE).transact().await?.into_result()?;
    }

    // Init escrow with 3 verifiers
    escrow.call("new")
        .args_json(json!({
            "verifier_set": [
                {"account_id": "v0.test.near", "public_key": hex_bytes(ska.verifying_key().as_bytes()), "active": true},
                {"account_id": "v1.test.near", "public_key": hex_bytes(skb.verifying_key().as_bytes()), "active": true},
                {"account_id": "v2.test.near", "public_key": hex_bytes(skc.verifying_key().as_bytes()), "active": true},
            ],
            "consensus_threshold": 2,
            "allowed_tokens": [ft.id()],
        }))
        .gas(GAS_INIT).transact().await?.into_result()?;

    Ok(TestEnv { worker, escrow, ft, agent, task_worker, ska, skb, skc })
}

async fn create_fund_claim_submit(env: &TestEnv, job_id: &str) -> anyhow::Result<String> {
    let amount = "1000000000000000000000";
    env.agent.call(env.escrow.id(), "create_escrow")
        .args_json(json!({"job_id": job_id, "amount": amount, "token": env.ft.id(),
            "timeout_hours": 24u64, "task_description": "Build widget", "criteria": "Works",
            "verifier_fee": null, "score_threshold": null, "max_submissions": null, "deadline_block": null}))
        .deposit(near_workspaces::types::NearToken::from_yoctonear(STORAGE_DEPOSIT))
        .gas(GAS_STORAGE).transact().await?.into_result()?;

    env.agent.call(env.ft.id(), "ft_transfer_call")
        .args_json(json!({"receiver_id": env.escrow.id(), "amount": amount, "msg": job_id}))
        .deposit(near_workspaces::types::NearToken::from_yoctonear(1))
        .gas(GAS_STORAGE).transact().await?.into_result()?;
    env.worker.fast_forward(5).await?;

    env.task_worker.call(env.escrow.id(), "claim")
        .args_json(json!({"job_id": job_id}))
        .deposit(near_workspaces::types::NearToken::from_yoctonear(WORKER_STAKE))
        .gas(GAS_CLAIM).transact().await?.into_result()?;

    env.task_worker.call(env.escrow.id(), "submit_result")
        .args_json(json!({"job_id": job_id, "result": "Widget built"}))
        .gas(GAS_SUBMIT).transact().await?.into_result()?;
    env.worker.fast_forward(5).await?;

    let verifying: Vec<serde_json::Value> = env.escrow.view("list_verifying")
        .args_json(json!({"from_index": 0, "limit": 10})).await?.json()?;
    let data_id = verifying.iter().find(|v| v["job_id"] == job_id)
        .and_then(|v| v["data_id"].as_str()).unwrap().to_string();
    Ok(data_id)
}

async fn resume_multi(env: &TestEnv, data_id_hex: &str, signed: serde_json::Value) -> near_workspaces::result::ExecutionFinalResult {
    let caller = env.worker.dev_create_account().await.unwrap();
    caller.call(env.escrow.id(), "resume_verification_multi")
        .args_json(json!({"data_id_hex": data_id_hex, "signed_verdict": signed}))
        .gas(GAS_RESUME).transact().await.unwrap()
}

// ============================================================
// Core multi-verifier tests
// ============================================================

#[tokio::test]
async fn test_happy_path_2of3() -> anyhow::Result<()> {
    let env = setup().await?;
    let did = create_fund_claim_submit(&env, "happy-1").await?;
    let vj = json!({"score": 88, "passed": true, "detail": "Consensus"}).to_string();
    let signed = build_signed(&did, &vj, &[(0, &env.ska), (1, &env.skb)]);

    let r = resume_multi(&env, &did, signed).await;
    assert!(r.is_success());

    env.worker.fast_forward(20).await?;
    let v: serde_json::Value = env.escrow.view("get_escrow").args_json(json!({"job_id": "happy-1"})).await?.json()?;
    assert_eq!(v["status"].as_str().unwrap(), "Claimed");
    assert_eq!(v["verdict"]["score"].as_u64().unwrap(), 88);
    println!("✅ 2-of-3 happy path: Claimed");
    Ok(())
}

#[tokio::test]
async fn test_insufficient_sigs() -> anyhow::Result<()> {
    let env = setup().await?;
    let did = create_fund_claim_submit(&env, "insuf-1").await?;
    let vj = json!({"score": 88, "passed": true, "detail": "One"}).to_string();
    let signed = build_signed(&did, &vj, &[(0, &env.ska)]);
    let r = resume_multi(&env, &did, signed).await;
    assert!(!r.is_success());
    println!("✅ Insufficient sigs rejected");
    Ok(())
}

#[tokio::test]
async fn test_forged_sig() -> anyhow::Result<()> {
    let env = setup().await?;
    let evil = gen_keypair_from(99);
    let did = create_fund_claim_submit(&env, "forged-1").await?;
    let vj = json!({"score": 88, "passed": true, "detail": "Forged"}).to_string();
    let sig_a = make_sig(&env.ska, &did, &vj, 0);
    let fake_b = make_sig(&evil, &did, &vj, 1);
    let signed = json!({"verdict_json": vj, "signatures": [sig_a, fake_b]});
    let r = resume_multi(&env, &did, signed).await;
    assert!(!r.is_success());
    println!("✅ Forged sig rejected");
    Ok(())
}

#[tokio::test]
async fn test_retry_after_rejection() -> anyhow::Result<()> {
    let env = setup().await?;
    let did = create_fund_claim_submit(&env, "retry-1").await?;
    let v1 = json!({"score": 90, "passed": true, "detail": "One"}).to_string();
    let r1 = resume_multi(&env, &did, build_signed(&did, &v1, &[(0, &env.ska)])).await;
    assert!(!r1.is_success());
    let v2 = json!({"score": 85, "passed": true, "detail": "OK"}).to_string();
    let r2 = resume_multi(&env, &did, build_signed(&did, &v2, &[(0, &env.ska), (1, &env.skb)])).await;
    assert!(r2.is_success());
    env.worker.fast_forward(20).await?;
    let v: serde_json::Value = env.escrow.view("get_escrow").args_json(json!({"job_id": "retry-1"})).await?.json()?;
    assert_eq!(v["status"].as_str().unwrap(), "Claimed");
    println!("✅ Retry after rejection works");
    Ok(())
}

#[tokio::test]
async fn test_sig_not_replayable() -> anyhow::Result<()> {
    let env = setup().await?;
    let did1 = create_fund_claim_submit(&env, "replay-1").await?;
    let did2 = create_fund_claim_submit(&env, "replay-2").await?;
    let vj = json!({"score": 90, "passed": true, "detail": "Test"}).to_string();
    let signed = build_signed(&did1, &vj, &[(0, &env.ska), (1, &env.skb)]);
    let r = resume_multi(&env, &did2, signed).await;
    assert!(!r.is_success());
    println!("✅ Sigs scoped — not replayable");
    Ok(())
}

#[tokio::test]
async fn test_too_many_sigs() -> anyhow::Result<()> {
    let env = setup().await?;
    let did = create_fund_claim_submit(&env, "spam-1").await?;
    let vj = json!({"score": 88, "passed": true, "detail": "Spam"}).to_string();
    let mut sigs = vec![];
    for _ in 0..5 { sigs.push(make_sig(&env.ska, &did, &vj, 0)); sigs.push(make_sig(&env.skb, &did, &vj, 1)); }
    let r = resume_multi(&env, &did, json!({"verdict_json": vj, "signatures": sigs})).await;
    assert!(!r.is_success());
    println!("✅ Too many sigs rejected");
    Ok(())
}

// ============================================================
// New prod features
// ============================================================

#[tokio::test]
async fn test_pause_blocks_create() -> anyhow::Result<()> {
    let env = setup().await?;

    // Pause
    env.escrow.call("pause").gas(GAS_INIT).transact().await?.into_result()?;
    let paused: bool = env.escrow.view("is_paused").await?.json()?;
    assert!(paused);

    // Create should fail
    let r = env.agent.call(env.escrow.id(), "create_escrow")
        .args_json(json!({"job_id": "paused-1", "amount": "1000", "token": env.ft.id(),
            "timeout_hours": 24u64, "task_description": "X", "criteria": "X",
            "verifier_fee": null, "score_threshold": null, "max_submissions": null, "deadline_block": null}))
        .deposit(near_workspaces::types::NearToken::from_yoctonear(STORAGE_DEPOSIT))
        .gas(GAS_STORAGE).transact().await?;
    assert!(!r.is_success());

    // Unpause
    env.escrow.call("unpause").gas(GAS_INIT).transact().await?.into_result()?;
    let paused2: bool = env.escrow.view("is_paused").await?.json()?;
    assert!(!paused2);
    println!("✅ Pause blocks create_escrow, unpause restores");
    Ok(())
}

#[tokio::test]
async fn test_owner_transfer() -> anyhow::Result<()> {
    let env = setup().await?;
    let new_owner = env.worker.dev_create_account().await?;

    // Step 1: propose
    env.escrow.call("propose_owner")
        .args_json(json!({"new_owner": new_owner.id()}))
        .gas(GAS_INIT).transact().await?.into_result()?;

    let pending: Option<String> = env.escrow.view("get_pending_owner").await?.json()?;
    assert_eq!(pending.unwrap(), new_owner.id().to_string());

    // Step 2: wrong account accepts → fail
    let random = env.worker.dev_create_account().await?;
    let r = random.call(env.escrow.id(), "accept_owner")
        .gas(GAS_INIT).transact().await?;
    assert!(!r.is_success());

    // Step 3: correct account accepts
    new_owner.call(env.escrow.id(), "accept_owner")
        .gas(GAS_INIT).transact().await?.into_result()?;

    let owner: String = env.escrow.view("get_owner").await?.json()?;
    assert_eq!(owner, new_owner.id().to_string());
    println!("✅ 2-step owner transfer works");
    Ok(())
}

#[tokio::test]
async fn test_token_whitelist() -> anyhow::Result<()> {
    let env = setup().await?;

    // FT is already whitelisted (passed in init). Try a non-whitelisted token.
    let fake_ft = env.worker.dev_create_account().await?;

    let r = env.agent.call(env.escrow.id(), "create_escrow")
        .args_json(json!({"job_id": "bad-token-1", "amount": "1000", "token": fake_ft.id(),
            "timeout_hours": 24u64, "task_description": "X", "criteria": "X",
            "verifier_fee": null, "score_threshold": null, "max_submissions": null, "deadline_block": null}))
        .deposit(near_workspaces::types::NearToken::from_yoctonear(STORAGE_DEPOSIT))
        .gas(GAS_STORAGE).transact().await?;
    assert!(!r.is_success(), "Non-whitelisted token should fail");

    // Add the token, retry
    env.escrow.call("add_allowed_token")
        .args_json(json!({"token": fake_ft.id()}))
        .gas(GAS_INIT).transact().await?.into_result()?;

    let r2 = env.agent.call(env.escrow.id(), "create_escrow")
        .args_json(json!({"job_id": "good-token-1", "amount": "1000", "token": fake_ft.id(),
            "timeout_hours": 24u64, "task_description": "X", "criteria": "X",
            "verifier_fee": null, "score_threshold": null, "max_submissions": null, "deadline_block": null}))
        .deposit(near_workspaces::types::NearToken::from_yoctonear(STORAGE_DEPOSIT))
        .gas(GAS_STORAGE).transact().await?;
    assert!(r2.is_success(), "Whitelisted token should work");

    println!("✅ Token whitelist enforced");
    Ok(())
}
