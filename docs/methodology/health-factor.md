# Health factor: methodology

## Definition
HF = ( sum over collateral assets of  balance_i * price_i * LT_i ) / ( sum over debt assets of  debt_j * price_j )

- Only assets with `usageAsCollateralEnabled` and LT > 0 count as collateral.
- Balances include accrued interest (scaled balance x index).
- Prices come from the protocol oracle (USD, 8 decimals on Aave v3).
- HF < 1 means the position can be liquidated.

## Worked example (fill in from scripts/verify_aave_hf.py output)
| Account | Block | Collateral USD (weighted by LT) | Debt USD | My HF | Protocol HF | Diff |
|---|---|---|---|---|---|---|
| 0x... | | | | | | |
| 0x... | | | | | | |
| 0x... | | | | | | |
| 0x... | | | | | | |
| 0x... | | | | | | |

## Things I learned / surprises
- (eMode accounts use category LT and price source: why my number was off)
- (rounding differences)
- (which accounts I skipped and why)

## Open questions
- 
