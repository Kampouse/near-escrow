#!/usr/bin/env python3
"""E2E test: Deploy escrow + msig contracts, post task, settle on NEAR testnet.

Uses direct RPC calls with nacl for signing (no near-api-py needed).
"""

import json
import time
import hashlib
import base64
import os
import sys
import struct
import subprocess
from pathlib import Path

from nacl.signing import SigningKey, VerifyKey
from nacl.exceptions import BadSignatureError
import requests
import base58

# ---- Config ----
RPC_URL = "https://rpc.testnet.near.org"
RPC_FAST = "https://test.rpc.fastnear.com"
CREDENTIALS_DIR = Path.home() / ".near-credentials" / "testnet"

# Load signer key
def load_account(account_id):
    path = CREDENTIALS_DIR / f"{account_id}.json"
    with open(path) as f:
        data = json.load(f)
    pk = data["private_key"]
    assert pk.startswith("ed25519:")
    sk_b58 = pk[len("ed25519:"):]
    sk_bytes = base58.b58decode(sk_b58)
    sk = SigningKey(sk_bytes[:32])
    return {"account_id": data["account_id"], "public_key": data["public_key"], "signing_key": sk}

# ---- RPC helpers ----
_nonce_counter = {}

def rpc_call(method, params, url=None):
    url = url or RPC_URL
    resp = requests.post(url, json={"jsonrpc":"2.0","id":1,"method":method,"params":params}, timeout=30)
    data = resp.json()
    if "error" in data:
        raise Exception(f"RPC error: {data['error']}")
    return data["result"]

def get_account_info(account_id):
    return rpc_call("query", {"request_type":"view_account","account_id":account_id,"finality":"optimistic"}, RPC_FAST)

def get_access_key(account_id, public_key):
    return rpc_call("query", {"request_type":"view_access_key","account_id":account_id,"public_key":public_key,"finality":"optimistic"}, RPC_FAST)

def get_block_hash(finality="optimistic"):
    r = rpc_call("block", {"finality": finality}, RPC_FAST)
    return r["header"]["hash"], r["header"]["height"]

def view_call(contract_id, method_name, args=b"", url=None):
    args_b64 = base64.b64encode(args if isinstance(args, bytes) else args.encode()).decode()
    return rpc_call("query", {"request_type":"call_function","account_id":contract_id,"method_name":method_name,"args_base64":args_b64,"finality":"optimistic"}, url or RPC_FAST)

def view_call_str(contract_id, method_name, args=b""):
    result = view_call(contract_id, method_name, args)
    return bytes(result["result"]).decode()

def get_nonce(account_id, public_key):
    info = get_access_key(account_id, public_key)
    return info["nonce"]

# ---- Transaction building ----
def build_transaction(signer_id, public_key, receiver_id, actions, block_hash, nonce=None):
    if nonce is None:
        nonce = get_nonce(signer_id, public_key)
    nonce = nonce + 1

    tx = json.dumps({
        "signerId": signer_id,
        "publicKey": public_key,
        "nonce": nonce,
        "receiverId": receiver_id,
        "blockHash": block_hash,
        "actions": actions,
    }, separators=(",", ":"))

    return tx, nonce

def sign_transaction(signer, receiver_id, actions):
    """Sign a transaction and return the signed tx bytes."""
    block_hash_b58, _ = get_block_hash()
    block_hash_bytes = base58.b58decode(block_hash_b58)
    
    nonce = get_nonce(signer["account_id"], signer["public_key"]) + 1
    
    # Build the transaction for signing
    # We need to serialize in the wire format
    # Instead, let's use the RPC's broadcast_tx_commit with JSON
    
    tx_data = {
        "signerId": signer["account_id"],
        "publicKey": signer["public_key"],
        "nonce": nonce,
        "receiverId": receiver_id,
        "blockHash": block_hash_b58,
        "actions": actions,
    }
    
    # Serialize and sign using borsh-compatible format
    # NEAR uses borsh for tx serialization - this is complex
    # Let's use a different approach: use near-cli (JS) or build via subprocess
    return tx_data, nonce, block_hash_b58

def send_tx(signer, receiver_id, actions):
    """Send a transaction using near-cli (JS) subprocess."""
    # Build the actions as JSON for near-cli
    # Actually, let's just use near-cli for the heavy lifting
    
    # For function calls, use: near call
    # For deployments, use: near deploy
    # For transfers, use: near send
    
    pass

# ---- Alternative: Use near-cli (JS) for all on-chain ops ----
def near_cli(args, timeout=60):
    """Run near-cli (JS) command."""
    cmd = ["npx", "near-cli"] + [str(a) for a in args]
    result = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
    return result.stdout, result.stderr, result.returncode

def near_call(contract_id, method, args, account_id, deposit="0", gas=300000000000000):
    """Call a contract method."""
    args_str = json.dumps(args) if isinstance(args, dict) else args
    stdout, stderr, rc = near_cli([
        "call", contract_id, method, args_str,
        "--accountId", account_id,
        "--deposit", str(deposit),
        "--gas", str(gas),
        "--networkId", "testnet",
    ])
    if rc != 0:
        raise Exception(f"near call failed: {stderr}")
    return stdout

def near_view(contract_id, method, args="{}"):
    """View a contract method."""
    args_str = json.dumps(args) if isinstance(args, dict) else args
    stdout, stderr, rc = near_cli([
        "view", contract_id, method, args_str,
        "--networkId", "testnet",
    ])
    if rc != 0:
        raise Exception(f"near view failed: {stderr}")
    return stdout

# ---- E2E Test ----
def main():
    print("=" * 60)
    print("E2E Test: NEAR Escrow + Inlayer Integration on Testnet")
    print("=" * 60)
    
    # 1. Load account
    print("\n[1/8] Loading account...")
    signer = load_account("kampy.testnet")
    main_account = signer["account_id"]
    print(f"  Account: {main_account}")
    print(f"  Public key: {signer['public_key']}")
    
    # Check balance
    try:
        info = get_account_info(main_account)
        balance = int(info["amount"]) / 1e24
        print(f"  Balance: {balance:.4f} NEAR")
        print(f"  Storage: {info['storage_usage']} bytes")
        if balance < 1:
            print("  ⚠️  Low balance! Need faucet funding.")
    except Exception as e:
        print(f"  ❌ Cannot read account: {e}")
        return
    
    # 2. Deploy escrow contract
    print("\n[2/8] Deploying escrow contract...")
    ESCROW_ID = "escrow.kampy.testnet"
    WASM_PATH = "/Users/asil/.openclaw/workspace/near-escrow/target/wasm32-unknown-unknown/release/near_escrow.wasm"
    MSIG_WASM = "/Users/asil/.openclaw/workspace/near-escrow/target/wasm32-unknown-unknown/release/agent_msig.wasm"
    
    # Check if escrow already deployed
    try:
        info = get_account_info(ESCROW_ID)
        code_hash = info.get("code_hash", "")
        if code_hash != "1111111111111111111111111111111111111111111111111111111111111111":
            print(f"  ✅ Escrow already deployed at {ESCROW_ID} (code_hash: {code_hash[:16]}...)")
        else:
            raise Exception("needs deploy")
    except:
        # Create subaccount and deploy
        print(f"  Creating {ESCROW_ID}...")
        stdout, stderr, rc = near_cli([
            "create-account", ESCROW_ID,
            "--accountId", main_account,
            "--initialBalance", "2",
            "--networkId", "testnet",
        ])
        if rc != 0 and "already exists" not in stderr:
            # Try kampy.testnet instead
            ESCROW_ID = "escrow-e2e.kampy.testnet"
            print(f"  Trying {ESCROW_ID} instead...")
            stdout, stderr, rc = near_cli([
                "create-account", ESCROW_ID,
                "--accountId", "kampy.testnet",
                "--initialBalance", "2",
                "--networkId", "testnet",
            ])
            if rc != 0:
                print(f"  ❌ Failed to create subaccount: {stderr}")
                return
        
        print(f"  Deploying escrow WASM...")
        stdout, stderr, rc = near_cli([
            "deploy", ESCROW_ID, WASM_PATH,
            "--networkId", "testnet",
            "--force",
        ])
        if rc != 0:
            print(f"  ❌ Deploy failed: {stderr}")
            return
        print(f"  ✅ Escrow deployed to {ESCROW_ID}")
    
    # 3. Deploy agent msig
    print("\n[3/8] Deploying agent-msig contracts...")
    AGENT_MSIG = "agent-e2e.kampy.testnet"
    WORKER_MSIG = "worker-e2e.kampy.testnet"
    
    # Generate agent key
    agent_sk = SigningKey.generate()
    agent_pk = agent_sk.verify_key.encode()
    agent_pk_b58 = base58.b58encode(agent_pk).decode()
    agent_pk_full = f"ed25519:{agent_pk_b58}"
    
    worker_sk = SigningKey.generate()
    worker_pk = worker_sk.verify_key.encode()
    worker_pk_b58 = base58.b58encode(worker_pk).decode()
    worker_pk_full = f"ed25519:{worker_pk_b58}"
    
    print(f"  Agent pubkey: {agent_pk_full}")
    print(f"  Worker pubkey: {worker_pk_full}")
    
    for msig_id, pk_label in [(AGENT_MSIG, "agent"), (WORKER_MSIG, "worker")]:
        try:
            info = get_account_info(msig_id)
            code_hash = info.get("code_hash", "")
            if code_hash != "1111111111111111111111111111111111111111111111111111111111111111":
                print(f"  ✅ {msig_id} already deployed")
                continue
        except:
            pass
        
        print(f"  Creating {msig_id}...")
        pk_to_use = agent_pk_full if pk_label == "agent" else worker_pk_full
        
        # Create with the msig's own key
        stdout, stderr, rc = near_cli([
            "create-account", msig_id,
            "--accountId", "kampy.testnet",
            "--initialBalance", "1",
            "--publicKey", pk_to_use,
            "--networkId", "testnet",
        ])
        if rc != 0:
            print(f"  ❌ Failed to create {msig_id}: {stderr}")
            return
        
        # Deploy msig WASM
        print(f"  Deploying msig WASM to {msig_id}...")
        stdout, stderr, rc = near_cli([
            "deploy", msig_id, MSIG_WASM,
            "--networkId", "testnet",
            "--force",
        ])
        if rc != 0:
            print(f"  ❌ Deploy failed: {stderr}")
            return
        
        # Initialize
        init_args = json.dumps({
            "agent_pubkey": pk_to_use,
            "agent_npub": "02" + pk_to_use,  # placeholder npub
            "escrow_contract": ESCROW_ID,
        })
        print(f"  Initializing {msig_id}...")
        stdout, stderr, rc = near_cli([
            "call", msig_id, "new", init_args,
            "--accountId", msig_id,
            "--networkId", "testnet",
        ])
        if rc != 0:
            print(f"  ❌ Init failed: {stderr}")
            return
        
        print(f"  ✅ {msig_id} deployed and initialized")
    
    # 4. Check escrow contract state
    print("\n[4/8] Checking escrow contract...")
    try:
        result = view_call_str(ESCROW_ID, "get_owner")
        print(f"  Escrow owner: {result}")
    except Exception as e:
        print(f"  Note: get_owner not available or not initialized: {e}")
    
    # 5. Build and submit CreateEscrow action
    print("\n[5/8] Creating escrow via msig.execute()...")
    
    # Get agent msig nonce
    try:
        nonce_result = view_call_str(AGENT_MSIG, "get_nonce")
        current_nonce = int(nonce_result.strip('"'))
        print(f"  Agent msig nonce: {current_nonce}")
    except Exception as e:
        print(f"  ❌ Cannot get nonce: {e}")
        return
    
    job_id = f"e2e-test-{int(time.time())}"
    next_nonce = current_nonce + 1
    
    action = {
        "nonce": next_nonce,
        "action": {
            "type": "create_escrow",
            "job_id": job_id,
            "amount": "1000000000000000000000000",  # 1 NEAR
            "token": "near",
            "timeout_hours": 24,
            "task_description": "E2E test: count to 10 and return the numbers",
            "criteria": "Must return numbers 1 through 10",
        }
    }
    action_json = json.dumps(action, separators=(",", ":"), sort_keys=True)
    
    # Sign with agent key
    signature = agent_sk.sign(action_json.encode())
    sig_bytes = signature.signature
    sig_list = list(sig_bytes)
    
    print(f"  Job ID: {job_id}")
    print(f"  Action JSON: {action_json[:100]}...")
    print(f"  Signature: {sig_bytes.hex()[:32]}...")
    
    # Submit via msig.execute()
    execute_args = json.dumps({
        "action_json": action_json,
        "signature": sig_list,
    })
    
    try:
        stdout, stderr, rc = near_cli([
            "call", AGENT_MSIG, "execute", execute_args,
            "--accountId", "kampy.testnet",
            "--deposit", "2000000000000000000000000",  # 2 NEAR for escrow storage
            "--gas", "300000000000000",
            "--networkId", "testnet",
        ])
        if rc != 0:
            print(f"  ❌ CreateEscrow failed: {stderr}")
            return
        print(f"  ✅ CreateEscrow submitted")
        print(f"  Output: {stdout[:200]}")
    except Exception as e:
        print(f"  ❌ Error: {e}")
        return
    
    # 6. Fund the escrow
    print("\n[6/8] Funding escrow...")
    next_nonce += 1
    fund_action = {
        "nonce": next_nonce,
        "action": {
            "type": "fund_escrow",
            "job_id": job_id,
            "amount": "1000000000000000000000000",  # 1 NEAR
        }
    }
    fund_action_json = json.dumps(fund_action, separators=(",", ":"), sort_keys=True)
    fund_sig = agent_sk.sign(fund_action_json.encode())
    fund_sig_list = list(fund_sig.signature)
    
    fund_execute_args = json.dumps({
        "action_json": fund_action_json,
        "signature": fund_sig_list,
    })
    
    try:
        stdout, stderr, rc = near_cli([
            "call", AGENT_MSIG, "execute", fund_execute_args,
            "--accountId", "kampy.testnet",
            "--deposit", "1000000000000000000000000",  # 1 NEAR
            "--gas", "300000000000000",
            "--networkId", "testnet",
        ])
        if rc != 0:
            print(f"  ❌ FundEscrow failed: {stderr}")
            return
        print(f"  ✅ FundEscrow submitted")
        print(f"  Output: {stdout[:200]}")
    except Exception as e:
        print(f"  ❌ Error: {e}")
        return
    
    # 7. Check escrow state
    print("\n[7/8] Checking escrow state...")
    try:
        result = view_call_str(ESCROW_ID, "get_escrow", json.dumps({"job_id": job_id}))
        print(f"  Escrow state: {result}")
    except Exception as e:
        print(f"  ❌ Cannot read escrow: {e}")
    
    # 8. Worker claims and submits result
    print("\n[8/8] Worker submitting result...")
    
    # Get worker nonce
    try:
        worker_nonce_result = view_call_str(WORKER_MSIG, "get_nonce")
        worker_nonce = int(worker_nonce_result.strip('"'))
    except Exception as e:
        print(f"  ❌ Cannot get worker nonce: {e}")
        return
    
    # Claim action
    claim_nonce = worker_nonce + 1
    claim_action = {
        "nonce": claim_nonce,
        "action": {
            "type": "claim_escrow",
            "job_id": job_id,
        }
    }
    claim_action_json = json.dumps(claim_action, separators=(",", ":"), sort_keys=True)
    claim_sig = worker_sk.sign(claim_action_json.encode())
    
    claim_args = json.dumps({
        "action_json": claim_action_json,
        "signature": list(claim_sig.signature),
    })
    
    try:
        stdout, stderr, rc = near_cli([
            "call", WORKER_MSIG, "execute", claim_args,
            "--accountId", "kampy.testnet",
            "--deposit", "100000000000000000000000",  # 0.1 NEAR stake
            "--gas", "300000000000000",
            "--networkId", "testnet",
        ])
        if rc != 0:
            print(f"  ⚠️  Claim result: {stderr[:200]}")
        else:
            print(f"  ✅ Claim submitted: {stdout[:200]}")
    except Exception as e:
        print(f"  ⚠️  Claim error: {e}")
    
    # Submit result action
    submit_nonce = claim_nonce + 1
    submit_action = {
        "nonce": submit_nonce,
        "action": {
            "type": "submit_result",
            "job_id": job_id,
            "result": "1, 2, 3, 4, 5, 6, 7, 8, 9, 10",
        }
    }
    submit_action_json = json.dumps(submit_action, separators=(",", ":"), sort_keys=True)
    submit_sig = worker_sk.sign(submit_action_json.encode())
    
    submit_args = json.dumps({
        "action_json": submit_action_json,
        "signature": list(submit_sig.signature),
    })
    
    try:
        stdout, stderr, rc = near_cli([
            "call", WORKER_MSIG, "execute", submit_args,
            "--accountId", "kampy.testnet",
            "--gas", "300000000000000",
            "--networkId", "testnet",
        ])
        if rc != 0:
            print(f"  ⚠️  Submit result: {stderr[:200]}")
        else:
            print(f"  ✅ Result submitted: {stdout[:200]}")
    except Exception as e:
        print(f"  ⚠️  Submit error: {e}")
    
    # Final escrow state check
    print("\n--- Final Escrow State ---")
    try:
        result = view_call_str(ESCROW_ID, "get_escrow", json.dumps({"job_id": job_id}))
        print(f"  {result}")
    except Exception as e:
        print(f"  Error: {e}")
    
    print("\n" + "=" * 60)
    print("E2E Test Complete")
    print("=" * 60)

if __name__ == "__main__":
    main()
