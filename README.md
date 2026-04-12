# near-escrow

Escrow smart contract for NEAR Protocol — agent-to-agent task payments with multi-verifier consensus.

## Modes

### Mode 1: Agent-to-Agent
Agent locks funds, worker accepts and completes the job, result is verified (hash check or multi-verifier vote), then payment is released.

### Mode 2: OutLayer (WASM Execution)
Agent locks funds and provides a WASM URL + input. The contract calls OutLayer for verifiable off-chain execution. Payment is released on success, refunded on failure.

## Features

- FT (fungible token) escrow with timeout and heartbeat monitoring
- Multi-verifier consensus with configurable threshold
- OutLayer cross-contract execution with callback
- Cancel/refund mechanics with role-based access
- View methods for querying escrows by agent, worker, or status

## Build

```bash
cargo build --target wasm32-unknown-unknown --release
```

## Contract Methods

### State-changing
- `create_escrow` — Lock funds for a job
- `accept` — Worker accepts the job
- `heartbeat` — Worker sends keepalive
- `submit_result` — Worker submits proof of work
- `verify` — Verifier votes on result
- `cancel` — Cancel and refund
- `refund` — Claim refund after timeout

### Read-only
- `get_escrow` — Get escrow by job ID
- `list_escrows_by_agent` — Filter by agent
- `list_escrows_by_worker` — Filter by worker
- `list_pending` — All active escrows
- `get_stats` — Contract-level stats
- `get_owner` / `get_outlayer_contract`

## License

MIT
