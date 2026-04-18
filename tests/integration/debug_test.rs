use near_workspaces::network::Sandbox;
use near_workspaces::Worker;

#[tokio::test]
async fn test_deploy_msig_as_escrow() -> anyhow::Result<()> {
    let worker: Worker<Sandbox> = near_workspaces::sandbox().await?;
    // Read msig wasm but deploy it as a new dev account (same as what works)
    let wasm = std::fs::read("../../target/wasm32-unknown-unknown/release/agent_msig.wasm")?;
    let contract = worker.dev_deploy(&wasm).await?;
    println!("msig deployed as: {}", contract.id());
    Ok(())
}

#[tokio::test]
async fn test_deploy_escrow_debug_build() -> anyhow::Result<()> {
    let worker: Worker<Sandbox> = near_workspaces::sandbox().await?;
    // Try reading the wasm with a full absolute path to rule out CWD issues
    let wasm = std::fs::read("/Users/asil/.openclaw/workspace/near-escrow/target/wasm32-unknown-unknown/release/near_escrow.wasm")?;
    println!("escrow wasm size: {} bytes", wasm.len());
    // Check the first 8 bytes (magic + version)
    println!("header: {:02x?}", &wasm[..8]);
    let contract = worker.dev_deploy(&wasm).await?;
    println!("escrow deployed OK: {}", contract.id());
    Ok(())
}
