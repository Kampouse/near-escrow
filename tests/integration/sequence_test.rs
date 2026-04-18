use near_workspaces::network::Sandbox;
use near_workspaces::Worker;

#[tokio::test]
async fn test_deploy_all_in_sequence() -> anyhow::Result<()> {
    let worker: Worker<Sandbox> = near_workspaces::sandbox().await?;
    
    // 1. Deploy msig first (this always works)
    let msig_wasm = std::fs::read("../../target/wasm32-unknown-unknown/release/agent_msig.wasm")?;
    let msig = worker.dev_deploy(&msig_wasm).await?;
    println!("1. msig deployed OK: {}", msig.id());
    
    // 2. Now deploy escrow (this usually fails)
    let escrow_wasm = std::fs::read("../../target/wasm32-unknown-unknown/release/near_escrow.wasm")?;
    println!("escrow wasm size: {} bytes", escrow_wasm.len());
    let escrow = worker.dev_deploy(&escrow_wasm).await?;
    println!("2. escrow deployed OK: {}", escrow.id());
    
    // 3. Now deploy ft-mock
    let ft_wasm = std::fs::read("../../target/wasm32-unknown-unknown/release/ft_mock.wasm")?;
    let ft = worker.dev_deploy(&ft_wasm).await?;
    println!("3. ft deployed OK: {}", ft.id());
    
    // 4. Initialize escrow
    escrow.call("new")
        .args_json(serde_json::json!({"verifier_account_id": Option::<String>::None}))
        .max_gas()
        .transact()
        .await?
        .into_result()?;
    println!("4. escrow initialized OK");
    
    Ok(())
}
