import json
import os
from decimal import Decimal
from pathlib import Path
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen


def load_api_key() -> str:
    env_path = Path(__file__).with_name(".env")
    for line in env_path.read_text().splitlines():
        line = line.strip()
        if line.startswith("export "):
            line = line[7:].lstrip()
        name, separator, value = line.partition("=")
        if separator and name.strip() == "ALCHEMY_API_KEY":
            return value.strip().strip("\"'")
    raise SystemExit("ALCHEMY_API_KEY is missing from analytics-app/.env")


def main() -> None:
    api_key = load_api_key()
    request = Request(
        f"https://eth-mainnet.g.alchemy.com/v2/{api_key}",
        data=json.dumps(
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "eth_getBalance",
                "params": ["0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045", "latest"],
            }
        ).encode(),
        headers={"Content-Type": "application/json"},
        method="POST",
    )

    try:
        with urlopen(request, timeout=20) as response:
            result = json.load(response)
    except HTTPError as error:
        raise SystemExit(f"Alchemy returned HTTP {error.code}") from None
    except URLError as error:
        reason = str(error.reason).replace(api_key, "[redacted]")
        raise SystemExit(f"Could not reach Alchemy: {reason}") from None

    if "error" in result:
        raise SystemExit(f"Alchemy JSON-RPC error: {result['error'].get('message', 'unknown error')}")

    balance_wei = int(result["result"], 16)
    balance_eth = Decimal(balance_wei) / Decimal(10**18)
    print(f"Vitalik's ETH balance: {balance_eth:f} ETH")


if __name__ == "__main__":
    main()
