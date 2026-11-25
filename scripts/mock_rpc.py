#!/usr/bin/env python3
"""
Mock Ethereum JSON-RPC server that speaks just enough of Aave v3 to exercise the real indexer binary
end to end WITHOUT an RPC key: blocks, logs (by range and by block hash), Multicall3 reads, and a
/admin/reorg endpoint that swaps in a different fork.

  python scripts/mock_rpc.py            # serves http://127.0.0.1:8545
  curl -X POST localhost:8545/admin/reorg      # replace the chain above block 32 with fork B
  curl localhost:8545/admin/truth              # ground-truth positions at the current head (JSON)

Needs:  pip install eth_abi "eth-hash[pycryptodome]" eth_utils
"""
import hashlib, json, sys, threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from eth_abi import decode, encode
from eth_utils import keccak, to_checksum_address

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 8545
ADDRESSES_PROVIDER = "0x2f39d218133AFaB8F2B819B1066c7E434Ad94E9e"
POOL = "0x00000000000000000000000000000000000000aa"
DATA_PROVIDER = "0x00000000000000000000000000000000000000dd"
MULTICALL3 = "0xca11bde05977b3631167028862be2a173976ca11"
WETH, USDC = "0x00000000000000000000000000000000000000e1", "0x00000000000000000000000000000000000000c1"
AWETH, AUSDC = "0x00000000000000000000000000000000000000a1", "0x00000000000000000000000000000000000000a2"
VWETH, VUSDC = "0x0000000000000000000000000000000000000071", "0x0000000000000000000000000000000000000072"
ALICE, BOB = "0x0000000000000000000000000000000000000a11", "0x0000000000000000000000000000000000000b0b"
RESERVES = [(WETH, AWETH, VWETH, "WETH", 18), (USDC, AUSDC, VUSDC, "USDC", 6)]
ATOKEN_OF = {WETH: AWETH, USDC: AUSDC}

def sel(sig): return keccak(text=sig)[:4]
def topic(sig): return "0x" + keccak(text=sig).hex()
def addr_topic(a): return "0x" + "00" * 12 + a[2:].lower()
def num_topic(n): return "0x" + format(n, "064x")
def h(b): return "0x" + b.hex()

EV = {
    "Supply": topic("Supply(address,address,address,uint256,uint16)"),
    "Borrow": topic("Borrow(address,address,address,uint256,uint8,uint256,uint16)"),
    "ReserveDataUpdated": topic("ReserveDataUpdated(address,uint256,uint256,uint256,uint256,uint256)"),
    "BalanceTransfer": topic("BalanceTransfer(address,address,uint256,uint256)"),
}

class Block:
    def __init__(self, number, salt, parent_hash, logs, deltas):
        self.number, self.salt, self.parent_hash = number, salt, parent_hash
        self.hash = "0x" + hashlib.sha256(f"{number}:{salt}".encode()).hexdigest()
        self.timestamp = 1_700_000_000 + number * 12
        for i, l in enumerate(logs):
            l.update(blockNumber=hex(number), blockHash=self.hash, logIndex=hex(i), transactionIndex="0x0",
                     transactionHash="0x" + hashlib.sha256(f"tx{number}:{salt}:{i}".encode()).hexdigest(), removed=False)
        self.logs, self.deltas = logs, deltas   # deltas: {(asset, account): (scaled_supply, scaled_debt, collateral)}

    def header_json(self):
        z32 = "0x" + "00" * 32
        return {"hash": self.hash, "parentHash": self.parent_hash, "sha3Uncles": z32, "miner": "0x" + "00" * 20,
                "stateRoot": z32, "transactionsRoot": z32, "receiptsRoot": z32, "logsBloom": "0x" + "00" * 256,
                "difficulty": "0x0", "number": hex(self.number), "gasLimit": "0x1c9c380", "gasUsed": "0x0",
                "timestamp": hex(self.timestamp), "extraData": "0x", "mixHash": z32, "nonce": "0x0000000000000000",
                "baseFeePerGas": "0x1", "size": "0x200", "transactions": [], "uncles": []}

def log(emitter, topics, data): return {"address": emitter, "topics": topics, "data": h(data)}
def supply(n, who, amount): return log(POOL, [EV["Supply"], addr_topic(WETH), addr_topic(who), num_topic(0)], encode(["address", "uint256"], [who, amount]))
def borrow(n, who, amount): return log(POOL, [EV["Borrow"], addr_topic(USDC), addr_topic(who), num_topic(0)], encode(["address", "uint256", "uint8", "uint256"], [who, amount, 2, 1]))
def reserve_update(n): return log(POOL, [EV["ReserveDataUpdated"], addr_topic(WETH)], encode(["uint256"] * 5, [10**25, 0, 2 * 10**25, 10**27 + n, 10**27 + 2 * n]))
def atoken_transfer(frm, to, v): return log(AWETH, [EV["BalanceTransfer"], addr_topic(frm), addr_topic(to)], encode(["uint256", "uint256"], [v, 10**27]))

def build_chain_a():
    chain = [Block(0, "g", "0x" + "00" * 32, [], {})]
    scripted = {  # block -> (logs, deltas)
        5:  ([supply(5, ALICE, 100)],               {(WETH, ALICE): (100, 0, True)}),
        9:  ([borrow(9, ALICE, 50)],                {(USDC, ALICE): (0, 50, False)}),
        20: ([supply(20, BOB, 10)],                 {(WETH, BOB): (10, 0, True)}),
        30: ([reserve_update(30)],                  {}),
        33: ([supply(33, BOB, 60), reserve_update(33)], {(WETH, BOB): (70, 0, True)}),
        35: ([atoken_transfer(ALICE, BOB, 40)],     {(WETH, ALICE): (60, 0, True), (WETH, BOB): (110, 0, True)}),
    }
    for n in range(1, 41):
        logs, deltas = scripted.get(n, ([], {}))
        chain.append(Block(n, "A", chain[-1].hash, logs, deltas))
    return chain

LOCK = threading.Lock()
CHAIN = build_chain_a()

def reorg():
    """Fork B: keep blocks <= 32, then a different history (Bob's supply never happened; Alice borrows more)."""
    global CHAIN
    with LOCK:
        base = CHAIN[:33]
        scripted = {34: ([borrow(34, ALICE, 999)], {(USDC, ALICE): (0, 999, False)}),
                    36: ([supply(36, BOB, 7)], {(WETH, BOB): (17, 0, True)})}
        for n in range(33, 39):
            logs, deltas = scripted.get(n, ([], {}))
            base.append(Block(n, "B", base[-1].hash, logs, deltas))
        CHAIN = base

def state_at(block_hash):
    by_hash = {b.hash: b for b in CHAIN}
    b = by_hash.get(block_hash) or CHAIN[-1]
    state = {}
    for blk in CHAIN[: b.number + 1]:
        state.update(blk.deltas)
    return state

def truth():
    s = state_at(CHAIN[-1].hash)
    return {f"{a}|{u}": [int(v[0]), int(v[1]), bool(v[2])] for (a, u), v in s.items()}

def call(to, data, block_hash):
    to = to.lower(); s4 = data[:4]
    if to == ADDRESSES_PROVIDER.lower():
        if s4 == sel("getPool()"): return encode(["address"], [POOL])
        if s4 == sel("getPoolDataProvider()"): return encode(["address"], [DATA_PROVIDER])
    if to == POOL and s4 == sel("getReservesList()"):
        return encode(["address[]"], [[r[0] for r in RESERVES]])
    if to == DATA_PROVIDER:
        if s4 == sel("getReserveTokensAddresses(address)"):
            (asset,) = decode(["address"], data[4:])
            r = next(x for x in RESERVES if x[0] == asset.lower())
            return encode(["address", "address", "address"], [r[1], "0x" + "00" * 20, r[2]])
        if s4 == sel("getUserReserveData(address,address)"):
            asset, user = decode(["address", "address"], data[4:])
            sup, debt, coll = state_at(block_hash).get((asset.lower(), user.lower()), (0, 0, False))
            return encode(["uint256", "uint256", "uint256", "uint256", "uint256", "uint256", "uint256", "uint40", "bool"],
                          [sup, 0, debt, 0, debt, 0, 0, 0, coll])
    for r in RESERVES:
        if to == r[0]:
            if s4 == sel("symbol()"): return encode(["string"], [r[3]])
            if s4 == sel("decimals()"): return encode(["uint8"], [r[4]])
        if to == r[1] and s4 == sel("scaledBalanceOf(address)"):
            (user,) = decode(["address"], data[4:])
            return encode(["uint256"], [state_at(block_hash).get((r[0], user.lower()), (0, 0, False))[0]])
    if to == MULTICALL3 and s4 == sel("aggregate3((address,bool,bytes)[])"):
        (calls,) = decode(["(address,bool,bytes)[]"], data[4:])
        out = [(True, call(t, cd, block_hash)) for (t, _allow, cd) in calls]
        return encode(["(bool,bytes)[]"], [out])
    raise ValueError(f"unhandled call to {to} selector {s4.hex()}")

def rpc(method, params):
    with LOCK:
        if method == "eth_chainId": return "0x1"
        if method == "eth_blockNumber": return hex(CHAIN[-1].number)
        if method == "eth_getBlockByNumber":
            n = params[0]
            blk = CHAIN[-1] if n == "latest" else (CHAIN[int(n, 16)] if int(n, 16) < len(CHAIN) else None)
            return blk.header_json() if blk else None
        if method == "eth_getLogs":
            f = params[0]
            addrs = f.get("address"); addrs = [addrs] if isinstance(addrs, str) else addrs
            t0 = (f.get("topics") or [[]])[0]; t0 = [t0] if isinstance(t0, str) else t0
            if "blockHash" in f: blocks = [b for b in CHAIN if b.hash == f["blockHash"]]
            else: blocks = [b for b in CHAIN if int(f["fromBlock"], 16) <= b.number <= int(f["toBlock"], 16)]
            return [l for b in blocks for l in b.logs
                    if (not addrs or l["address"].lower() in [a.lower() for a in addrs]) and (not t0 or l["topics"][0] in t0)]
        if method == "eth_call":
            tx, blk = params[0], params[1] if len(params) > 1 else "latest"
            bh = blk.get("blockHash") if isinstance(blk, dict) else None
            raw = tx.get("input") or tx.get("data")
            return h(call(tx["to"], bytes.fromhex(raw[2:]), bh))
    raise ValueError(f"unsupported method {method}")

class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def _send(self, obj, code=200):
        body = json.dumps(obj).encode()
        self.send_response(code); self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body)
    def do_GET(self):
        if self.path == "/admin/truth": return self._send(truth())
        self._send({"error": "not found"}, 404)
    def do_POST(self):
        if self.path == "/admin/reorg":
            reorg(); return self._send({"ok": True, "head": CHAIN[-1].number})
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        def one(req):
            try: return {"jsonrpc": "2.0", "id": req["id"], "result": rpc(req["method"], req.get("params", []))}
            except Exception as e:
                print("RPC ERROR", req.get("method"), e, file=sys.stderr)
                return {"jsonrpc": "2.0", "id": req["id"], "error": {"code": -32000, "message": str(e)}}
        self._send([one(r) for r in body] if isinstance(body, list) else one(body))

if __name__ == "__main__":
    print(f"mock Aave RPC on http://127.0.0.1:{PORT}  (head {CHAIN[-1].number})", flush=True)
    ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
