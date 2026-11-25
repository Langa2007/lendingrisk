#!/usr/bin/env python3
"""
Phase 1 exit check: does the indexer's database match the chain?

  python scripts/verify_state.py              # 1000 random positions
  python scripts/verify_state.py 200          # smaller sample
  python scripts/verify_state.py 1000 --coverage-blocks 300

What it checks (all at ONE pinned block = the indexer's cursor, so run it while the indexer is idle
or caught up; if the chain moved on, the pin keeps the comparison fair):
  1. SAMPLE  : N random rows from `position` -> compare scaled supply, scaled debt and collateral flag
               with the chain (aToken.scaledBalanceOf, PoolDataProvider.getUserReserveData).
  2. COVERAGE: accounts that supplied/borrowed in the last K blocks before the cursor must exist in
               `position`. This catches MISSED EVENTS, which a database-to-chain sample cannot see.

Needs:  pip install web3 "psycopg[binary]"      (both are in engine/pyproject.toml)
Env  :  DATABASE_URL, RPC_URL, CHAIN_ID (default 1), AAVE_V3_ADDRESSES_PROVIDER, LOG_CHUNK (default 500)
Exit :  0 = all match, 1 = mismatches, 2 = setup problem
"""
import argparse
import os
import sys
from pathlib import Path

import psycopg
from web3 import Web3

MULTICALL3 = Web3.to_checksum_address("0xcA11bde05977b3631167028862bE2a173976CA11")
ZERO = "0x" + "00" * 20


def load_env(path: Path) -> None:
    if path.exists():
        for line in path.read_text().splitlines():
            line = line.strip()
            if line and not line.startswith("#") and "=" in line:
                k, v = line.split("=", 1)
                os.environ.setdefault(k.strip(), v.strip())


load_env(Path(__file__).resolve().parent.parent / ".env")


def fn(name, ins, outs):
    return {"type": "function", "name": name, "stateMutability": "view",
            "inputs": [{"name": f"i{k}", "type": t} for k, t in enumerate(ins)],
            "outputs": [{"name": f"o{k}", "type": t} for k, t in enumerate(outs)]}


PROVIDER_ABI = [fn("getPool", [], ["address"]), fn("getPoolDataProvider", [], ["address"])]
DP_ABI = [fn("getUserReserveData", ["address", "address"],
             ["uint256", "uint256", "uint256", "uint256", "uint256", "uint256", "uint256", "uint40", "bool"])]
ATOKEN_ABI = [fn("scaledBalanceOf", ["address"], ["uint256"])]
MC_ABI = [{
    "type": "function", "name": "aggregate3", "stateMutability": "payable",
    "inputs": [{"name": "calls", "type": "tuple[]", "components": [
        {"name": "target", "type": "address"}, {"name": "allowFailure", "type": "bool"},
        {"name": "callData", "type": "bytes"}]}],
    "outputs": [{"name": "returnData", "type": "tuple[]", "components": [
        {"name": "success", "type": "bool"}, {"name": "returnData", "type": "bytes"}]}],
}]


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("sample", nargs="?", type=int, default=1000)
    ap.add_argument("--coverage-blocks", type=int, default=300, help="0 disables the coverage check")
    args = ap.parse_args()

    rpc, dsn = os.environ.get("RPC_URL", "").split(",")[0], os.environ.get("DATABASE_URL")
    chain_id = int(os.environ.get("CHAIN_ID", "1"))
    provider_addr = os.environ.get("AAVE_V3_ADDRESSES_PROVIDER", "0x2f39d218133AFaB8F2B819B1066c7E434Ad94E9e")
    chunk = int(os.environ.get("LOG_CHUNK", "500"))
    if not rpc or "YOUR_KEY" in rpc or not dsn:
        print("Set RPC_URL and DATABASE_URL in .env (see .env.example).")
        return 2

    w3 = Web3(Web3.HTTPProvider(rpc, request_kwargs={"timeout": 60}))
    conn = psycopg.connect(dsn)
    cur = conn.cursor()

    cur.execute("SELECT block_number, block_hash FROM processed_block WHERE chain_id=%s ORDER BY block_number DESC LIMIT 1", (chain_id,))
    row = cur.fetchone()
    if not row:
        print("processed_block is empty: run the indexer first.")
        return 2
    block, db_hash = int(row[0]), row[1]
    canon = w3.eth.get_block(block)["hash"].hex()
    canon = canon if canon.startswith("0x") else "0x" + canon
    print(f"cursor block {block}   db hash {db_hash[:14]}...   chain hash {canon[:14]}...")
    if canon.lower() != db_hash.lower():
        print("FAIL: the block the indexer recorded as its cursor is NOT canonical (reorg not yet repaired?). Wait for the indexer and retry.")
        return 1

    cur.execute("SELECT asset, atoken FROM reserve_token WHERE protocol='aave_v3' AND chain_id=%s", (chain_id,))
    atoken = {a.lower(): Web3.to_checksum_address(t) for a, t in cur.fetchall()}
    if not atoken:
        print("reserve_token is empty: the indexer has not initialised.")
        return 2

    provider = w3.eth.contract(address=Web3.to_checksum_address(provider_addr), abi=PROVIDER_ABI)
    dp_addr = provider.functions.getPoolDataProvider().call(block_identifier=block)
    pool_addr = provider.functions.getPool().call(block_identifier=block)
    dp = w3.eth.contract(address=dp_addr, abi=DP_ABI)
    mc = w3.eth.contract(address=MULTICALL3, abi=MC_ABI)

    # ---------------- 1. sample: DB -> chain ----------------
    cur.execute("""SELECT asset, account, scaled_supply::text, scaled_debt::text, collateral_enabled, last_block
                   FROM position WHERE protocol='aave_v3' AND chain_id=%s AND last_block <= %s
                   ORDER BY random() LIMIT %s""", (chain_id, block, args.sample))
    rows = cur.fetchall()
    cur.execute("SELECT count(*) FROM position WHERE protocol='aave_v3' AND chain_id=%s", (chain_id,))
    total = cur.fetchone()[0]
    print(f"positions in DB: {total}; checking a random sample of {len(rows)}")

    mismatches, checked = [], 0
    for i in range(0, len(rows), 100):
        batch = rows[i:i + 100]
        calls = []
        for asset, account, *_ in batch:
            a, u = Web3.to_checksum_address(asset), Web3.to_checksum_address(account)
            token = w3.eth.contract(address=atoken[asset.lower()], abi=ATOKEN_ABI)
            calls.append((atoken[asset.lower()], False, token.encode_abi("scaledBalanceOf", args=[u])))
            calls.append((dp_addr, False, dp.encode_abi("getUserReserveData", args=[a, u])))
        res = mc.functions.aggregate3(calls).call(block_identifier=block)
        for j, (asset, account, s_sup, s_debt, coll, last_block) in enumerate(batch):
            ok_bal, ret_bal = res[2 * j]
            ok_dp, ret_dp = res[2 * j + 1]
            if not (ok_bal and ok_dp):
                mismatches.append((asset, account, "call failed on chain"))
                continue
            chain_sup = int.from_bytes(ret_bal[:32], "big")
            words = [int.from_bytes(ret_dp[k * 32:(k + 1) * 32], "big") for k in range(9)]
            chain_debt, chain_coll = words[4], bool(words[8])
            db_sup, db_debt = int(s_sup), int(s_debt)
            checked += 1
            if (db_sup, db_debt, coll) != (chain_sup, chain_debt, chain_coll):
                mismatches.append((asset, account,
                                   f"db(sup={db_sup}, debt={db_debt}, coll={coll}, last_block={last_block}) "
                                   f"!= chain(sup={chain_sup}, debt={chain_debt}, coll={chain_coll})"))
    print(f"SAMPLE  : {checked - len([m for m in mismatches if 'db(' in m[2]])}/{checked} match")

    # ---------------- 2. coverage: chain events -> DB ----------------
    missing = []
    if args.coverage_blocks > 0:
        t_supply = "0x" + Web3.keccak(text="Supply(address,address,address,uint256,uint16)").hex().removeprefix("0x")
        t_borrow = "0x" + Web3.keccak(text="Borrow(address,address,address,uint256,uint8,uint256,uint16)").hex().removeprefix("0x")
        lo = max(0, block - args.coverage_blocks + 1)
        seen = set()
        start = lo
        while start <= block:
            end = min(start + chunk - 1, block)
            logs = w3.eth.get_logs({"address": pool_addr, "fromBlock": start, "toBlock": end, "topics": [[t_supply, t_borrow]]})
            for lg in logs:
                topics = [t.hex() if hasattr(t, "hex") else t for t in lg["topics"]]
                topics = ["0x" + t.removeprefix("0x") for t in topics]
                seen.add(("0x" + topics[1][-40:], "0x" + topics[2][-40:]))   # (reserve, onBehalfOf)
            start = end + 1
        for asset, account in sorted(seen):
            cur.execute("SELECT 1 FROM position WHERE protocol='aave_v3' AND chain_id=%s AND asset=%s AND account=%s",
                        (chain_id, asset.lower(), account.lower()))
            if cur.fetchone() is None:
                missing.append((asset, account))
        print(f"COVERAGE: {len(seen) - len(missing)}/{len(seen)} accounts active in blocks {lo}-{block} are in the DB")

    bad = len([m for m in mismatches]) + len(missing)
    for asset, account, why in mismatches[:20]:
        print(f"  MISMATCH {account} {asset[:10]}.. {why}")
    for asset, account in missing[:20]:
        print(f"  MISSING  {account} {asset[:10]}.. (event seen on chain, not in DB)")
    print("RESULT  :", "PASS" if bad == 0 else f"FAIL ({bad} problems)")
    return 0 if bad == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
