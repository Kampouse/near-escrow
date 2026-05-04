# Task Manifest Spec

## Problem

The escrow contract stores `task_description` and `criteria` as plain strings. There's no structured way to define WHAT work to do, HOW to execute it, or HOW to judge the result. This makes the system useless for anything beyond "I'll pay if you say the right thing."

This spec defines a **task manifest** — a JSON document pinned to IPFS/Arweave that the escrow contract references by CID. The contract stays dumb (payment routing + signature checking). All task intelligence lives in the manifest.

## Design Principles

1. **Contract stays minimal** — it stores a CID, not the manifest itself
2. **Language-agnostic** — works for Lisp, Python, compiled binaries, or non-code tasks
3. **Reproducible verification** — given the same manifest + same input, any verifier gets the same answer
4. **Composable** — manifest templates for common task types (ML training, code review, translation, etc.)
5. **Upgradeable** — manifest format can evolve without contract changes (it's off-chain)

---

## Manifest Schema

```jsonc
{
  // --- IDENTITY ---
  "version": 1,                          // manifest format version
  "name": "Train sentiment model",       // human-readable task name
  "task_type": "code",                   // code | document | data | prompt | mixed

  // --- INPUT ---
  "input": {
    "ref": "ipfs://Qm...",               // CID or URL pointing to input data
    "description": "Labeled tweets dataset (50k rows)",
    "format": "csv",                     // csv | json | zip | git | text | binary | none
    "hash": "sha256:abc123...",          // integrity check (optional but recommended)
    "size_bytes": 12500000               // for gas/storage estimation
  },

  // --- EXECUTION (how to DO the work) ---
  "execution": {
    "method": "container",               // container | script | manual | none
    "runtime": "python:3.12-slim",       // container image or interpreter
    "entrypoint": "python train.py --epochs 10",
    "timeout_seconds": 600,
    "memory_mb": 2048,
    "cpu_cores": 2,
    "network": false,                    // sandbox: no outbound network
    "env": {                             // injected into runtime
      "DATA_PATH": "/input/dataset.csv",
      "OUTPUT_PATH": "/output/model.pkl"
    },
    "artifacts": [                       // what the worker must produce
      {"path": "/output/model.pkl", "description": "Trained model weights"},
      {"path": "/output/metrics.json", "description": "Accuracy/F1 scores"}
    ]
  },

  // --- VERIFICATION (how to JUDGE the work) ---
  "verification": {
    "method": "test_suite",              // deterministic | test_suite | llm_judge | human_review
    "runtime": "python:3.12-slim",       // verifier may use different runtime
    "entrypoint": "pytest verify.py -v",
    "timeout_seconds": 120,
    "criteria": "All tests pass. F1 > 0.85.",
    "threshold": 0.85,                   // minimum score (for llm_judge, 0.0-1.0)
    "judge_prompt": null,                // for llm_judge: evaluation prompt template
    "reviewers": null,                   // for human_review: list of reviewer account IDs
    "verify_hash": "sha256:a3f2b8c1d4e5f6..."
    // SHA-256 hash of verify/ directory contents (agent computes at task creation)
  },

  // --- PAYMENT ---
  "payment": {
    "amount": "50000000000000000000",     // yoctoNEAR or FT amount (informational)
    "token": "usdt.tonictest.near",      // FT contract (informational, contract has its own)
    "verifier_fee_bps": 500,             // verifier fee in basis points (5%)
    "split": "winner_takes_all"          // winner_takes_all | proportional
  },

  // --- METADATA ---
  "tags": ["ml", "nlp", "training"],
  "created_at": "2026-05-04T13:00:00Z",
  "created_by": "jemartel.near"
}
```

---

## Task Types

| Type | Description | Example |
|------|-------------|---------|
| `code` | Write, fix, or run code | "Fix bug in parser" |
| `document` | Write text, research, translate | "Write blog post about X" |
| `data` | Process, transform, analyze data | "Clean dataset and compute stats" |
| `prompt` | Creative or subjective AI work | "Generate marketing copy" |
| `mixed` | Combination of the above | "Research topic + write report" |

---

## Verification Methods

### 1. Deterministic

Re-run the worker's submission in an identical sandbox. Compare output hash.

```json
{
  "method": "deterministic",
  "runtime": "python:3.12-slim",
  "entrypoint": "python solve.py",
  "expected_hash": "sha256:expected_output_hash"
}
```

Use for: math, data transforms, pure functions, compiler outputs.
Trust level: cryptographic. Output either matches or it doesn't.

### 2. Test Suite

Worker submits code/artifacts. Verifier runs a test suite against them.

```json
{
  "method": "test_suite",
  "runtime": "python:3.12-slim",
  "entrypoint": "pytest tests/ -v",
  "criteria": "All tests pass"
}
```

Use for: bug fixes, feature implementations, code quality.
Trust level: high. Tests are defined by the agent, run by the verifier.

### 3. LLM Judge

An LLM scores the output against criteria on a 0-1 scale.

```json
{
  "method": "llm_judge",
  "criteria": "Accurate, well-structured, under 500 words",
  "threshold": 0.8,
  "judge_prompt": "Rate this output on accuracy (0.4 weight), clarity (0.3), and conciseness (0.3). Return a JSON with score 0-1 and reasoning."
}
```

Use for: writing, research, translation, creative work.
Trust level: moderate. Judge model must be agreed upon. Scores can vary.

### 4. Human Review

Multisig of trusted reviewers approve or reject.

```json
{
  "method": "human_review",
  "criteria": "Design is professional and on-brand",
  "reviewers": ["reviewer1.near", "reviewer2.near", "reviewer3.near"],
  "threshold": 0.67                     // 2 of 3 must approve
}
```

Use for: subjective work, high-value tasks, disputes.
Trust level: social. Depends on reviewer reputation.

---

## Execution Methods

| Method | Description | Sandbox |
|--------|-------------|---------|
| `container` | Run in Docker/Podman container | Full isolation, resource limits |
| `script` | Run a shell script directly | Process-level isolation |
| `manual` | No automated execution | None — worker does work externally |
| `none` | No execution needed | N/A — for document/prompt tasks |

---

## End-to-End Flow

```
1. AGENT
   - Writes task manifest (JSON)
   - Pins input data to IPFS
   - Pins manifest to IPFS
   - Gets manifest CID: QmManifest...

2. ESCROW CREATION
   - Calls create_escrow() with manifest_cid as task_reference
   - Funds escrow with FT

3. WORKER
   - Browses open escrows
   - Pulls manifest from IPFS (QmManifest...)
   - Pulls input data from IPFS
   - Executes per manifest.execution
   - Produces artifacts
   - Pins artifacts to IPFS
   - Submits artifact CID to escrow

4. VERIFIER SERVICE
   - Watches for escrows in Verifying state
   - Pulls manifest + worker artifacts from IPFS
   - Runs manifest.verification.method
   - Produces verdict: {score, passed, detail}
   - Signs verdict with ed25519
   - Submits to escrow

5. CONTRACT
   - Checks verifier signature
   - If passed → settle (pay worker + verifier)
   - If failed → refund agent
   - If timeout → refund agent
```

---

## Contract Changes

Minimal. The contract stores a CID instead of (or alongside) the raw strings.

### New field on Escrow struct

```rust
/// CID of task manifest pinned to IPFS/Arweave
task_manifest_cid: Option<String>,    // max 128 bytes, e.g. "Qm..."
```

### Modified create_escrow params

```rust
pub fn create_escrow(
    &mut self,
    job_id: String,
    amount: U128,
    token: AccountId,
    timeout_hours: u64,
    task_manifest_cid: String,         // NEW: replaces task_description + criteria
    task_description: Option<String>,  // DEPRECATED: kept for backward compat, auto-populated from manifest
    criteria: Option<String>,          // DEPRECATED: kept for backward compat
    verifier_fee: Option<U128>,
    score_threshold: Option<u8>,
    max_submissions: Option<u32>,
    deadline_block: Option<u64>,
)
```

### What doesn't change

- Verification flow (verifier signs, contract checks sig, settles)
- Settlement logic (split between worker + verifier)
- Timeout/refund mechanics
- Competitive mode
- Worker stake/anti-spam

The contract still doesn't know or care what's IN the manifest. It's just a reference. The verifier service is the one that reads and interprets the manifest.

---

## Manifest Templates

Common task types get pre-built templates. Agents fill in the blanks.

### Code Fix
```json
{
  "task_type": "code",
  "input": {"ref": "ipfs://Qm...bug_report.json", "format": "json"},
  "execution": {"method": "container", "runtime": "rust:1.75", "entrypoint": "cargo test"},
  "verification": {"method": "test_suite", "entrypoint": "cargo test --all"}
}
```

### ML Training
```json
{
  "task_type": "code",
  "input": {"ref": "ipfs://Qm...dataset.zip", "format": "zip"},
  "execution": {"method": "container", "runtime": "python:3.12", "entrypoint": "python train.py"},
  "verification": {"method": "test_suite", "entrypoint": "pytest eval.py", "threshold": 0.85}
}
```

### Content Writing
```json
{
  "task_type": "document",
  "execution": {"method": "none"},
  "verification": {"method": "llm_judge", "criteria": "Accurate, engaging, SEO-optimized", "threshold": 0.75}
}
```

### Translation
```json
{
  "task_type": "document",
  "input": {"ref": "ipfs://Qm...source.txt", "format": "text"},
  "execution": {"method": "none"},
  "verification": {"method": "llm_judge", "criteria": "Faithful translation, natural tone", "threshold": 0.8}
}
```

### Research
```json
{
  "task_type": "prompt",
  "execution": {"method": "manual"},
  "verification": {"method": "llm_judge", "criteria": "Comprehensive, cited sources, structured", "threshold": 0.7}
}
```

---

## Security Considerations

1. **Manifest immutability** — once pinned to IPFS, the manifest can't change. If the agent needs to modify it, they create a new version and a new escrow.

2. **Input integrity** — `input.hash` lets workers and verifiers verify they pulled the right data. SHA-256 of the input file.

3. **Sandbox escape** — containers run with no network, resource limits, read-only filesystem (except /output). This isn't a full security boundary but sufficient for verification.

4. **Verifier trust** — the verifier service is trusted to run the correct verification method. The manifest is public, so anyone can audit what should have been run.

5. **LLM judge manipulation** — prompts can be gamed. Threshold helps but isn't foolproof. For high-value tasks, use test_suite or human_review.

6. **CID collision** — IPFS uses SHA-256, collision probability is negligible.

---

## Open Questions

- **Who pins the manifest?** Agent pins, or does the escrow contract/multisig handle it? (Recommendation: agent pins, contract just stores CID)
- **Who pays for IPFS pinning?** Agent, or subsidized by the platform? Pinning costs are small (~$0.01/GB/month) but not zero.
- **Manifest registry?** Should there be an on-chain registry of manifest CIDs for discoverability? Or is IPFS search sufficient?
- **Versioning?** If a manifest needs revision (e.g. fix a typo in criteria), create new CID + new escrow? Or allow manifest updates within a window?
- **Verifier selection?** How does the agent choose which verifier service to use? On-chain registry of verifiers with reputation scores?
