use near_workspaces::network::Sandbox;
use near_workspaces::Worker;

#[tokio::test]
async fn test_minimal_deploy() -> anyhow::Result<()> {
    let worker: Worker<Sandbox> = near_workspaces::sandbox().await?;
    let wasm = std::fs::read("../../target/wasm32-unknown-unknown/release/agent_msig.wasm")?;
    let contract = worker.dev_deploy(&wasm).await?;
    contract.call("new")
        .args_json(serde_json::json!({
            "agent_pubkey": "ed25519:11111111111111111111111111111111",
            "agent_npub": "test",
            "escrow_contract": "escrow.test.near",
        }))
        .max_gas()
        .transact()
        .await?
        .into_result()?;
    
    println!("msig-via-minimal-test deployed OK: {}", contract.id());
    Ok(())
}
