use near_workspaces::network::Sandbox;
use near_workspaces::Worker;
use serde_json::json;

#[tokio::test]
async fn test_deploy_escrow_only() -> anyhow::Result<()> {
    let worker: Worker<Sandbox> = near_workspaces::sandbox().await?;
    let wasm = std::fs::read("../../target/wasm32-unknown-unknown/release/near_escrow.wasm")?;
    let contract = worker.dev_deploy(&wasm).await?;
    contract.call("new").args_json(json!({
        "verifier_set": [{"account_id": "verifier.test.near", "public_key": "0000000000000000000000000000000000000000000000000000000000000000", "active": true}],
        "consensus_threshold": 1,
        "allowed_tokens": ["usdt.tether-token.near"]
    })).max_gas().transact().await?.into_result()?;
    println!("escrow deployed OK: {}", contract.id());
    Ok(())
}

#[tokio::test]
async fn test_deploy_msig_only() -> anyhow::Result<()> {
    let worker: Worker<Sandbox> = near_workspaces::sandbox().await?;
    let wasm = std::fs::read("../../target/wasm32-unknown-unknown/release/agent_msig.wasm")?;
    let contract = worker.dev_deploy(&wasm).await?;
    println!("msig deployed OK: {}", contract.id());
    Ok(())
}

#[tokio::test]
async fn test_deploy_ft_only() -> anyhow::Result<()> {
    let worker: Worker<Sandbox> = near_workspaces::sandbox().await?;
    let wasm = std::fs::read("../../target/wasm32-unknown-unknown/release/ft_mock.wasm")?;
    let contract = worker.dev_deploy(&wasm).await?;
    contract.call("new").max_gas().transact().await?.into_result()?;
    println!("ft deployed OK: {}", contract.id());
    Ok(())
}
