use near_workspaces::network::Sandbox;
use near_workspaces::Worker;

#[tokio::test]
async fn test_msig_actually_executes() -> anyhow::Result<()> {
    let worker: Worker<Sandbox> = near_workspaces::sandbox().await?;
    
    let msig_wasm = std::fs::read("../../target/wasm32-unknown-unknown/release/agent_msig.wasm")?;
    let msig = worker.dev_deploy(&msig_wasm).await?;
    println!("1. msig deployed: {}", msig.id());
    
    // Try to actually CALL a function on msig
    let result = msig.call("get_owner").view().await;
    println!("2. get_owner result: {:?}", result);
    
    // Also try new()
    let result = msig.call("new")
        .args_json(serde_json::json!({
            "owners": vec!["dev-account.test.near"],
            "num_confirmations": 1u64
        }))
        .max_gas()
        .transact().await;
    println!("3. new result: {:?}", result);
    
    Ok(())
}

#[tokio::test]
async fn test_escrow_actually_executes() -> anyhow::Result<()> {
    let worker: Worker<Sandbox> = near_workspaces::sandbox().await?;
    
    let escrow_wasm = std::fs::read("../../target/wasm32-unknown-unknown/release/near_escrow.wasm")?;
    let escrow = worker.dev_deploy(&escrow_wasm).await?;
    println!("1. escrow deployed: {}", escrow.id());
    
    // Try calling new()
    let result = escrow.call("new").max_gas().transact().await;
    println!("2. new result: {:?}", result);
    
    Ok(())
}
