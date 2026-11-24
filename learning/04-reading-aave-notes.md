# Reading Aave v3 contracts: notes

Repo: github.com/aave-dao/aave-v3-origin (or aave/aave-v3-core for the classic layout). Read in this order and answer the questions in your own words.

1. `contracts/protocol/libraries/logic/GenericLogic.sol` -> `calculateUserAccountData`
   - How is the weighted average liquidation threshold computed? Where does the rounding happen?
   - How does eMode change the LT and the price source?
2. `contracts/protocol/libraries/logic/ReserveLogic.sol` -> `getNormalizedIncome`, `getNormalizedDebt`
   - How do the indices grow between events? Why does debt change with no event emitted?
3. `contracts/protocol/libraries/logic/ValidationLogic.sol` -> `validateLiquidationCall`
   - Which conditions make a liquidation valid? What is the HF threshold for the 100% close factor?
4. `contracts/protocol/libraries/logic/LiquidationLogic.sol` -> `executeLiquidationCall`
   - How are bonus and close factor applied? What happens when collateral is smaller than debt x (1 + bonus)?
5. `contracts/protocol/libraries/math/WadRayMath.sol` and `PercentageMath.sol`
   - What do wad (1e18) and ray (1e27) mean? Which direction does each rounding function round?

## My answers
(write here)

## Events the indexer must decode (list them as you find them)
- Supply, Withdraw, Borrow, Repay, LiquidationCall, ReserveDataUpdated, ReserveUsedAsCollateralEnabled/Disabled, UserEModeSet
