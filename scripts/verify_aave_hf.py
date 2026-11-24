#!/usr/bin/env python3
"""
Phase 0 exercise: recompute Aave v3 health factors BY HAND and compare with the protocol.

Setup (inside the engine venv):   pip install web3
Run (from repo root):             python scripts/verify_aave_hf.py            # 5 accounts
                                  python scripts/verify_aave_hf.py 10         # 10 accounts

Needs RPC_URL in .env (see .env.example). Read-only: it only makes eth_call / eth_getLogs requests.
"""
import os
import sys
from pathlib import Path

from web3 import Web3


def load_env(path: Path) -> None:
    if not path.exists():
        return
    for line in path.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        k, v = line.split("=", 1)
        os.environ.setdefault(k.strip(), v.strip())


load_env(Path(__file__).resolve().parent.parent / ".env")

RPC_URL = os.environ.get("RPC_URL", "")
PROVIDER = os.environ.get("AAVE_V3_ADDRESSES_PROVIDER", "0x2f39d218133AFaB8F2B819B1066c7E434Ad94E9e")
LOG_CHUNK = int(os.environ.get("LOG_CHUNK", "500"))
MAX_CHUNKS = 400
TOLERANCE = 1e-4  # relative difference allowed (rounding in the protocol's integer math)
N_ACCOUNTS = int(sys.argv[1]) if len(sys.argv) > 1 else 5


def fn(name, ins, outs):
    return {
        "type": "function",
        "name": name,
        "stateMutability": "view",
        "inputs": [{"name": f"i{k}", "type": t} for k, t in enumerate(ins)],
        "outputs": [{"name": f"o{k}", "type": t} for k, t in enumerate(outs)],
    }


PROVIDER_ABI = [
    fn("getPool", [], ["address"]),
    fn("getPoolDataProvider", [], ["address"]),
    fn("getPriceOracle", [], ["address"]),
]
POOL_ABI = [
    fn("getUserAccountData", ["address"], ["uint256"] * 6),
    fn("getReservesList", [], ["address[]"]),
    fn("getUserEMode", ["address"], ["uint256"]),
]
DATA_PROVIDER_ABI = [
    fn(
        "getReserveConfigurationData",
        ["address"],
        ["uint256", "uint256", "uint256", "uint256", "uint256", "bool", "bool", "bool", "bool", "bool"],
    ),
    fn(
        "getUserReserveData",
        ["address", "address"],
        ["uint256", "uint256", "uint256", "uint256", "uint256", "uint256", "uint256", "uint40", "bool"],
    ),
]
ORACLE_ABI = [fn("getAssetPrice", ["address"], ["uint256"])]
ERC20_ABI = [fn("symbol", [], ["string"])]


def main() -> int:
    if not RPC_URL or "YOUR_KEY" in RPC_URL:
        print("Set RPC_URL in .env first (copy .env.example).")
        return 2

    w3 = Web3(Web3.HTTPProvider(RPC_URL, request_kwargs={"timeout": 30}))
    chain_id = w3.eth.chain_id
    if chain_id != 1:
        print(f"Warning: chain id is {chain_id}, expected 1 (Ethereum mainnet).")
    block = w3.eth.block_number - 2  # pin every call to ONE block so numbers are comparable
    print(f"chain {chain_id}, pinned block {block}")

    provider = w3.eth.contract(address=Web3.to_checksum_address(PROVIDER), abi=PROVIDER_ABI)
    pool_addr = provider.functions.getPool().call(block_identifier=block)
    dp_addr = provider.functions.getPoolDataProvider().call(block_identifier=block)
    oracle_addr = provider.functions.getPriceOracle().call(block_identifier=block)
    print(f"pool {pool_addr}\ndata provider {dp_addr}\noracle {oracle_addr}\n")

    pool = w3.eth.contract(address=pool_addr, abi=POOL_ABI)
    dp = w3.eth.contract(address=dp_addr, abi=DATA_PROVIDER_ABI)
    oracle = w3.eth.contract(address=oracle_addr, abi=ORACLE_ABI)

    reserves = pool.functions.getReservesList().call(block_identifier=block)
    print(f"{len(reserves)} reserves listed")

    sym_cache, cfg_cache, price_cache = {}, {}, {}

    def symbol(asset):
        if asset not in sym_cache:
            try:
                c = w3.eth.contract(address=asset, abi=ERC20_ABI)
                sym_cache[asset] = c.functions.symbol().call(block_identifier=block)
            except Exception:
                sym_cache[asset] = asset[:8]
        return sym_cache[asset]

    def cfg(asset):
        if asset not in cfg_cache:
            c = dp.functions.getReserveConfigurationData(asset).call(block_identifier=block)
            cfg_cache[asset] = {"decimals": c[0], "lt_bps": c[2], "active": c[8]}
        return cfg_cache[asset]

    def price(asset):  # USD, 8 decimals
        if asset not in price_cache:
            price_cache[asset] = oracle.functions.getAssetPrice(asset).call(block_identifier=block)
        return price_cache[asset]

    # ---- find accounts that recently borrowed (position owner = onBehalfOf, topic index 2) ----
    sig = "Borrow(address,address,address,uint256,uint8,uint256,uint16)"
    topic0 = "0x" + bytes(Web3.keccak(text=sig)).hex()
    users, seen = [], set()
    hi = block
    for _ in range(MAX_CHUNKS):
        lo = max(0, hi - LOG_CHUNK + 1)
        try:
            logs = w3.eth.get_logs({"address": pool_addr, "fromBlock": lo, "toBlock": hi, "topics": [topic0]})
        except Exception as e:
            print(f"get_logs failed for {lo}-{hi}: {e}\nLower LOG_CHUNK in .env and retry.")
            return 2
        for lg in reversed(logs):
            addr = Web3.to_checksum_address(bytes(lg["topics"][2])[-20:])
            if addr not in seen:
                seen.add(addr)
                users.append(addr)
        if len(users) >= N_ACCOUNTS * 6:
            break
        hi = lo - 1
    print(f"{len(users)} candidate borrowers found\n")

    done = ok = skipped = 0
    for user in users:
        if done >= N_ACCOUNTS:
            break
        acct = pool.functions.getUserAccountData(user).call(block_identifier=block)
        total_debt_base = acct[1]
        if total_debt_base == 0:
            continue  # already repaid
        emode = pool.functions.getUserEMode(user).call(block_identifier=block)
        if emode != 0:
            skipped += 1
            print(f"{user}  eMode category {emode}: skipped (eMode changes LT and prices; see learning notes)")
            continue

        weighted_collateral = 0.0
        debt = 0.0
        rows = []
        for asset in reserves:
            u = dp.functions.getUserReserveData(asset, user).call(block_identifier=block)
            a_bal, stable_debt, var_debt, use_as_coll = u[0], u[1], u[2], u[8]
            if a_bal == 0 and stable_debt == 0 and var_debt == 0:
                continue
            c = cfg(asset)
            p = price(asset) / 1e8
            dec = 10 ** c["decimals"]
            if a_bal and use_as_coll and c["lt_bps"] > 0 and c["active"]:
                v = a_bal / dec * p
                weighted_collateral += v * c["lt_bps"] / 10000
                rows.append(f"    collateral {symbol(asset):>8}  {a_bal/dec:>18.6f}  x ${p:,.2f}  x LT {c['lt_bps']/100:.2f}%")
            d = (stable_debt + var_debt) / dec
            if d:
                debt += d * p
                rows.append(f"    debt       {symbol(asset):>8}  {d:>18.6f}  x ${p:,.2f}")

        my_hf = weighted_collateral / debt if debt else float("inf")
        protocol_hf = acct[5] / 1e18
        diff = abs(my_hf - protocol_hf) / protocol_hf if protocol_hf else float("inf")
        good = diff < TOLERANCE
        done += 1
        ok += good
        print(f"{user}")
        print("\n".join(rows))
        print(f"    weighted collateral ${weighted_collateral:,.2f} / debt ${debt:,.2f}")
        print(f"    my HF {my_hf:.6f}   protocol HF {protocol_hf:.6f}   rel diff {diff:.2e}   {'PASS' if good else 'FAIL'}\n")

    print(f"{ok}/{done} accounts matched within {TOLERANCE:.0e} ({skipped} eMode accounts skipped)")
    if done == 0:
        print("No accounts compared. Try again, or raise N.")
        return 1
    return 0 if ok == done else 1


if __name__ == "__main__":
    sys.exit(main())
