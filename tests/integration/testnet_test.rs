use near_workspaces::network::Testnet;
use near_workspaces::Worker;

#[tokio::test]
async fn test_deploy_on_testnet() -> anyhow::Result<()> {
    // Use testnet - this will deploy to real testnet
    let worker: Worker<Testnet> = near_workspaces::testnet().await?;
    
    let escrow_wasm = std::fs::read("../../target/wasm32-unknown-unknown/release/near_escrow.wasm")?;
    println!("escrow wasm size: {} bytes", escrow_wasm.len());
    let contract = worker.dev_deploy(&escrow_wasm).await?;
    println!("escrow deployed: {}", contract.id());
    
    // Initialize (new takes Option<AccountId>, pass null for None)
    contract.call("new")
        .args_json(serde_json::json!({ "verifier_account_id": null }))
        .max_gas()
        .transact().await?.into_result()?;
    println!("escrow initialized OK");
    
    Ok(())
}
