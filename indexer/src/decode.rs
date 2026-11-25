//! Turns raw logs into the facts the indexer needs. Pure: no I/O, easy to test.
//!
//! Design: events tell us WHICH (asset, account) positions changed in a block.
//! The exact scaled balances are then READ from the chain at that block (see indexer.rs),
//! so we never accumulate rounding errors from reconstructing balances out of event amounts.
use crate::abi::*;
use crate::types::*;
use alloy::primitives::{Address, B256};
use alloy::sol_types::SolEvent;
use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Default, Clone)]
pub struct Decoded {
    /// (asset, account) pairs whose scaled balances or collateral flag may have changed
    pub touches: BTreeSet<(Address, Address)>,
    /// last ReserveDataUpdated per asset in the block
    pub reserve_updates: Vec<ReserveUpdate>,
    pub emodes: Vec<(Address, u8)>,
    pub liquidations: Vec<LiquidationRow>,
    pub oracle_updates: Vec<OracleUpdate>,
}

/// All topic0 values we subscribe to.
pub fn topics0() -> Vec<B256> {
    vec![
        MintUnbacked::SIGNATURE_HASH,
        Supply::SIGNATURE_HASH,
        Withdraw::SIGNATURE_HASH,
        Borrow::SIGNATURE_HASH,
        Repay::SIGNATURE_HASH,
        LiquidationCall::SIGNATURE_HASH,
        ReserveUsedAsCollateralEnabled::SIGNATURE_HASH,
        ReserveUsedAsCollateralDisabled::SIGNATURE_HASH,
        ReserveDataUpdated::SIGNATURE_HASH,
        UserEModeSet::SIGNATURE_HASH,
        SwapBorrowRateMode::SIGNATURE_HASH,
        RebalanceStableBorrowRate::SIGNATURE_HASH,
        BalanceTransfer::SIGNATURE_HASH,
        AnswerUpdated::SIGNATURE_HASH,
    ]
}

pub trait ProtocolAdapter: Send + Sync {
    fn topics0(&self) -> Vec<B256>;
    fn decode(&self, logs: &[RawLog], tokens: &TokenMap) -> Result<Decoded>;
}

pub struct AaveV3Adapter;

impl ProtocolAdapter for AaveV3Adapter {
    fn topics0(&self) -> Vec<B256> {
        topics0()
    }

    fn decode(&self, logs: &[RawLog], tokens: &TokenMap) -> Result<Decoded> {
        decode_logs(logs, tokens)
    }
}

macro_rules! dec {
    ($ty:ty, $log:expr) => {
        <$ty>::decode_raw_log($log.topics.iter().copied(), &$log.data).with_context(|| {
            format!(
                "decoding {} at block {} log {}",
                stringify!($ty),
                $log.block_number,
                $log.log_index
            )
        })?
    };
}

pub fn decode_logs(logs: &[RawLog], tokens: &TokenMap) -> Result<Decoded> {
    let mut d = Decoded::default();
    let mut reserve_last: BTreeMap<Address, ReserveUpdate> = BTreeMap::new();

    for log in logs {
        let Some(t0) = log.topics.first() else {
            continue;
        };
        if *t0 == MintUnbacked::SIGNATURE_HASH {
            let e = dec!(MintUnbacked, log);
            d.touches.insert((e.reserve, e.onBehalfOf));
        } else if *t0 == Supply::SIGNATURE_HASH {
            let e = dec!(Supply, log);
            d.touches.insert((e.reserve, e.onBehalfOf));
        } else if *t0 == Withdraw::SIGNATURE_HASH {
            let e = dec!(Withdraw, log);
            d.touches.insert((e.reserve, e.user));
        } else if *t0 == Borrow::SIGNATURE_HASH {
            let e = dec!(Borrow, log);
            d.touches.insert((e.reserve, e.onBehalfOf));
        } else if *t0 == Repay::SIGNATURE_HASH {
            let e = dec!(Repay, log);
            d.touches.insert((e.reserve, e.user)); // debt of `user` shrinks
            if e.useATokens {
                d.touches.insert((e.reserve, e.repayer)); // repayer's aTokens are burned
            }
        } else if *t0 == LiquidationCall::SIGNATURE_HASH {
            let e = dec!(LiquidationCall, log);
            d.touches.insert((e.collateralAsset, e.user));
            d.touches.insert((e.debtAsset, e.user));
            d.liquidations.push(LiquidationRow {
                collateral_asset: e.collateralAsset,
                debt_asset: e.debtAsset,
                account: e.user,
                debt_repaid: e.debtToCover,
                collateral_seized: e.liquidatedCollateralAmount,
                liquidator: e.liquidator,
                tx_hash: log.tx_hash,
                log_index: log.log_index,
            });
            // If the liquidator receives aTokens, an aToken BalanceTransfer is also emitted and handled below.
        } else if *t0 == ReserveUsedAsCollateralEnabled::SIGNATURE_HASH {
            let e = dec!(ReserveUsedAsCollateralEnabled, log);
            d.touches.insert((e.reserve, e.user));
        } else if *t0 == ReserveUsedAsCollateralDisabled::SIGNATURE_HASH {
            let e = dec!(ReserveUsedAsCollateralDisabled, log);
            d.touches.insert((e.reserve, e.user));
        } else if *t0 == ReserveDataUpdated::SIGNATURE_HASH {
            let e = dec!(ReserveDataUpdated, log);
            let upd = ReserveUpdate {
                asset: e.reserve,
                liquidity_rate: e.liquidityRate,
                variable_borrow_rate: e.variableBorrowRate,
                liquidity_index: e.liquidityIndex,
                variable_borrow_index: e.variableBorrowIndex,
                log_index: log.log_index,
            };
            match reserve_last.get(&e.reserve) {
                Some(prev) if prev.log_index > upd.log_index => {}
                _ => {
                    reserve_last.insert(e.reserve, upd);
                }
            }
        } else if *t0 == UserEModeSet::SIGNATURE_HASH {
            let e = dec!(UserEModeSet, log);
            d.emodes.push((e.user, e.categoryId));
        } else if *t0 == SwapBorrowRateMode::SIGNATURE_HASH {
            let e = dec!(SwapBorrowRateMode, log);
            d.touches.insert((e.reserve, e.user));
        } else if *t0 == RebalanceStableBorrowRate::SIGNATURE_HASH {
            let e = dec!(RebalanceStableBorrowRate, log);
            d.touches.insert((e.reserve, e.user));
        } else if *t0 == BalanceTransfer::SIGNATURE_HASH {
            // Emitted by an aToken contract; its address tells us which reserve.
            if let Some(asset) = tokens.asset_for_atoken(&log.address) {
                let e = dec!(BalanceTransfer, log);
                d.touches.insert((asset, e.from));
                d.touches.insert((asset, e.to));
            }
        } else if *t0 == AnswerUpdated::SIGNATURE_HASH {
            let e = dec!(AnswerUpdated, log);
            let updated_at = e
                .updatedAt
                .try_into()
                .context("Chainlink updatedAt exceeds u64")?;
            d.oracle_updates.push(OracleUpdate {
                feed: log.address,
                round_id: e.roundId,
                answer: e.current.to_string(),
                updated_at,
                log_index: log.log_index,
            });
        }
    }
    d.reserve_updates = reserve_last.into_values().collect();
    Ok(d)
}

#[cfg(test)]
pub mod test_logs {
    //! Helpers to build RawLogs from typed events (used by tests).
    use super::*;
    use alloy::primitives::{Bytes, U256};

    pub fn raw<E: SolEvent>(ev: &E, emitter: Address, block: u64, log_index: u64) -> RawLog {
        let data = ev.encode_log_data();
        RawLog {
            address: emitter,
            topics: data.topics().to_vec(),
            data: Bytes::from(data.data.to_vec()),
            block_number: block,
            tx_hash: B256::repeat_byte(block as u8),
            log_index,
        }
    }

    pub fn supply(reserve: Address, user: Address, on_behalf: Address, amount: u64) -> Supply {
        Supply {
            reserve,
            user,
            onBehalfOf: on_behalf,
            amount: U256::from(amount),
            referralCode: 0,
        }
    }
    pub fn borrow(reserve: Address, user: Address, on_behalf: Address, amount: u64) -> Borrow {
        Borrow {
            reserve,
            user,
            onBehalfOf: on_behalf,
            amount: U256::from(amount),
            interestRateMode: 2,
            borrowRate: U256::from(1u64),
            referralCode: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_logs::*;
    use super::*;
    use alloy::primitives::{address, I256, U256};

    const POOL: Address = address!("00000000000000000000000000000000000000aa");
    const WETH: Address = address!("00000000000000000000000000000000000000e1");
    const USDC: Address = address!("00000000000000000000000000000000000000c1");
    const ATOKEN_WETH: Address = address!("00000000000000000000000000000000000000a1");
    const ALICE: Address = address!("0000000000000000000000000000000000000a11");
    const BOB: Address = address!("0000000000000000000000000000000000000b0b");
    const CAROL: Address = address!("0000000000000000000000000000000000000ca0");

    fn tokens() -> TokenMap {
        TokenMap::from_reserves(vec![ReserveTokens {
            asset: WETH,
            atoken: ATOKEN_WETH,
            variable_debt_token: Address::repeat_byte(0x77),
            symbol: "WETH".into(),
            decimals: 18,
        }])
    }

    #[test]
    fn supply_touches_on_behalf_of_not_caller() {
        let logs = vec![raw(&supply(WETH, ALICE, BOB, 5), POOL, 10, 0)];
        let d = decode_logs(&logs, &tokens()).unwrap();
        assert_eq!(d.touches.len(), 1);
        assert!(d.touches.contains(&(WETH, BOB)));
    }

    #[test]
    fn borrow_touches_on_behalf_of() {
        let logs = vec![raw(&borrow(USDC, ALICE, BOB, 5), POOL, 10, 0)];
        let d = decode_logs(&logs, &tokens()).unwrap();
        assert!(d.touches.contains(&(USDC, BOB)));
        assert!(!d.touches.contains(&(USDC, ALICE)));
    }

    #[test]
    fn repay_with_atokens_touches_repayer_too() {
        let ev = Repay {
            reserve: USDC,
            user: ALICE,
            repayer: BOB,
            amount: U256::from(1u64),
            useATokens: true,
        };
        let d = decode_logs(&[raw(&ev, POOL, 1, 0)], &tokens()).unwrap();
        assert!(d.touches.contains(&(USDC, ALICE)) && d.touches.contains(&(USDC, BOB)));
        let ev2 = Repay {
            useATokens: false,
            ..ev
        };
        let d2 = decode_logs(&[raw(&ev2, POOL, 1, 0)], &tokens()).unwrap();
        assert_eq!(d2.touches.len(), 1);
    }

    #[test]
    fn liquidation_touches_both_assets_and_records_row() {
        let ev = LiquidationCall {
            collateralAsset: WETH,
            debtAsset: USDC,
            user: ALICE,
            debtToCover: U256::from(100u64),
            liquidatedCollateralAmount: U256::from(7u64),
            liquidator: CAROL,
            receiveAToken: false,
        };
        let d = decode_logs(&[raw(&ev, POOL, 3, 4)], &tokens()).unwrap();
        assert!(d.touches.contains(&(WETH, ALICE)) && d.touches.contains(&(USDC, ALICE)));
        assert_eq!(d.liquidations.len(), 1);
        assert_eq!(d.liquidations[0].liquidator, CAROL);
        assert_eq!(d.liquidations[0].log_index, 4);
    }

    #[test]
    fn atoken_balance_transfer_touches_both_sides_via_emitter_address() {
        let ev = BalanceTransfer {
            from: ALICE,
            to: BOB,
            value: U256::from(1u64),
            index: U256::from(1u64),
        };
        let d = decode_logs(&[raw(&ev, ATOKEN_WETH, 5, 0)], &tokens()).unwrap();
        assert!(d.touches.contains(&(WETH, ALICE)) && d.touches.contains(&(WETH, BOB)));
        // Emitted by an unknown contract: ignored
        let d2 = decode_logs(&[raw(&ev, Address::repeat_byte(9), 5, 0)], &tokens()).unwrap();
        assert!(d2.touches.is_empty());
    }

    #[test]
    fn last_reserve_update_in_block_wins() {
        let mk = |idx: u64| ReserveDataUpdated {
            reserve: WETH,
            liquidityRate: U256::from(1u64),
            stableBorrowRate: U256::ZERO,
            variableBorrowRate: U256::from(2u64),
            liquidityIndex: U256::from(idx),
            variableBorrowIndex: U256::from(idx),
        };
        let logs = vec![raw(&mk(100), POOL, 1, 0), raw(&mk(200), POOL, 1, 3)];
        let d = decode_logs(&logs, &tokens()).unwrap();
        assert_eq!(d.reserve_updates.len(), 1);
        assert_eq!(d.reserve_updates[0].liquidity_index, U256::from(200u64));
    }

    #[test]
    fn malformed_known_log_is_an_error_not_silently_skipped() {
        let mut bad = raw(&supply(WETH, ALICE, BOB, 5), POOL, 1, 0);
        bad.data = alloy::primitives::Bytes::from(vec![1u8, 2, 3]);
        assert!(decode_logs(&[bad], &tokens()).is_err());
    }

    #[test]
    fn unrelated_topic_is_ignored() {
        let log = RawLog {
            address: POOL,
            topics: vec![B256::repeat_byte(0x42)],
            data: Default::default(),
            block_number: 1,
            tx_hash: B256::ZERO,
            log_index: 0,
        };
        assert!(decode_logs(&[log], &tokens()).unwrap().touches.is_empty());
    }

    #[test]
    fn chainlink_answer_updates_are_decoded() {
        let feed = Address::repeat_byte(0x44);
        let event = AnswerUpdated {
            current: I256::try_from(U256::from(123u64)).unwrap(),
            roundId: U256::from(9u64),
            updatedAt: U256::from(1_700_000_000u64),
        };
        let decoded = decode_logs(&[raw(&event, feed, 10, 4)], &tokens()).unwrap();
        assert_eq!(decoded.oracle_updates.len(), 1);
        assert_eq!(decoded.oracle_updates[0].feed, feed);
        assert_eq!(decoded.oracle_updates[0].round_id, U256::from(9u64));
        assert_eq!(decoded.oracle_updates[0].answer, "123");
    }

    #[test]
    fn less_common_aave_events_touch_affected_positions() {
        let minted = MintUnbacked {
            reserve: WETH,
            user: ALICE,
            onBehalfOf: BOB,
            amount: U256::from(1u64),
            referralCode: 0,
        };
        let swapped = SwapBorrowRateMode {
            reserve: USDC,
            user: CAROL,
            interestRateMode: 2,
        };
        let rebalanced = RebalanceStableBorrowRate {
            reserve: USDC,
            user: ALICE,
        };
        let decoded = decode_logs(
            &[
                raw(&minted, POOL, 1, 0),
                raw(&swapped, POOL, 1, 1),
                raw(&rebalanced, POOL, 1, 2),
            ],
            &tokens(),
        )
        .unwrap();
        assert!(decoded.touches.contains(&(WETH, BOB)));
        assert!(decoded.touches.contains(&(USDC, CAROL)));
        assert!(decoded.touches.contains(&(USDC, ALICE)));
    }
}
