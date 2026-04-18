use near_workspaces::network::Sandbox;
use near_workspaces::Worker;

#[tokio::test]
async fn test_escrow_no_init() -> anyhow::Result<()> {
    let worker: Worker<Sandbox> = near_workspaces::sandbox().await?;
    
    let escrow_wasm = std::fs::read("../../target/wasm32-unknown-unknown/release/near_escrow.wasm")?;
    println!("escrow wasm size: {} bytes", escrow_wasm.len());
    let escrow = worker.dev_deploy(&escrow_wasm).await?;
    println!("escrow deployed OK: {}", escrow.id());
    
    // Try a view call instead of a mutable call
    let result = escrow.call("get_owner").view().await;
    println!("get_owner result: {:?}", result);
    
    // Now try init WITHOUT max_gas
    let result = escrow.call("new").transact().await;
    println!("new result: {:?}", result);
    
    Ok(())
}
