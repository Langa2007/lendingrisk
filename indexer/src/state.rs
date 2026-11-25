//! Aave v3 interest-index math, ported from MathUtils / WadRayMath so we can turn
//! scaled balances into real balances at ANY timestamp (interest accrues with no event).
//! Used by the Phase 2 health engine; unit-tested here so the math is proven before it is relied on.
#![allow(dead_code)]
use alloy::primitives::{uint, U256};

pub const RAY: U256 = uint!(1_000_000_000_000_000_000_000_000_000_U256);
pub const HALF_RAY: U256 = uint!(500_000_000_000_000_000_000_000_000_U256);
pub const SECONDS_PER_YEAR: u64 = 31_536_000;

/// (a * b + HALF_RAY) / RAY, rounding half up like Aave's rayMul. None on overflow.
pub fn ray_mul(a: U256, b: U256) -> Option<U256> {
    let p = a.checked_mul(b)?.checked_add(HALF_RAY)?;
    Some(p / RAY)
}

/// (a * RAY + b/2) / b, like Aave's rayDiv. None on overflow or b == 0.
pub fn ray_div(a: U256, b: U256) -> Option<U256> {
    if b.is_zero() {
        return None;
    }
    let p = a.checked_mul(RAY)?.checked_add(b / U256::from(2))?;
    Some(p / b)
}

/// 1 + rate * dt / year (linear, used for supply-side income)
pub fn linear_interest(rate: U256, dt: u64) -> Option<U256> {
    let r = rate.checked_mul(U256::from(dt))? / U256::from(SECONDS_PER_YEAR);
    RAY.checked_add(r)
}

/// Aave's 3-term binomial approximation of (1 + rate/year)^dt (used for variable debt).
pub fn compounded_interest(rate: U256, dt: u64) -> Option<U256> {
    if dt == 0 {
        return Some(RAY);
    }
    let spy = U256::from(SECONDS_PER_YEAR);
    let exp = U256::from(dt);
    let exp_m1 = exp - U256::from(1);
    let exp_m2 = if dt > 2 {
        exp - U256::from(2)
    } else {
        U256::ZERO
    };
    let base_pow2 = ray_mul(rate, rate)? / (spy * spy);
    let base_pow3 = ray_mul(base_pow2, rate)? / spy;
    let second = exp.checked_mul(exp_m1)?.checked_mul(base_pow2)? / U256::from(2);
    let third = exp
        .checked_mul(exp_m1)?
        .checked_mul(exp_m2)?
        .checked_mul(base_pow3)?
        / U256::from(6);
    let first = rate.checked_mul(exp)? / spy;
    RAY.checked_add(first)?
        .checked_add(second)?
        .checked_add(third)
}

/// Supply index "now": liquidityIndex grown linearly since the reserve's last update.
pub fn normalized_income(
    liquidity_index: U256,
    liquidity_rate: U256,
    last_update: u64,
    now: u64,
) -> Option<U256> {
    if now <= last_update {
        return Some(liquidity_index);
    }
    ray_mul(
        linear_interest(liquidity_rate, now - last_update)?,
        liquidity_index,
    )
}

/// Variable-debt index "now": variableBorrowIndex grown by compound interest since last update.
pub fn normalized_debt(
    borrow_index: U256,
    variable_rate: U256,
    last_update: u64,
    now: u64,
) -> Option<U256> {
    if now <= last_update {
        return Some(borrow_index);
    }
    ray_mul(
        compounded_interest(variable_rate, now - last_update)?,
        borrow_index,
    )
}

/// Real (underlying-unit) balance from a scaled balance and a normalized index.
pub fn scaled_to_real(scaled: U256, normalized_index: U256) -> Option<U256> {
    ray_mul(scaled, normalized_index)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pct(p: u64) -> U256 {
        RAY * U256::from(p) / U256::from(100)
    }

    #[test]
    fn ray_mul_identity_and_rounding() {
        let x = U256::from(123_456_789u64);
        assert_eq!(ray_mul(x, RAY).unwrap(), x);
        // 0.5 * 1 (in ray) rounds half up
        assert_eq!(ray_mul(U256::from(1), HALF_RAY).unwrap(), U256::from(1));
        assert_eq!(
            ray_mul(U256::from(1), HALF_RAY - U256::from(1)).unwrap(),
            U256::ZERO
        );
    }

    #[test]
    fn ray_div_roundtrip() {
        let a = U256::from(1_000_000u64);
        let idx = RAY * U256::from(11) / U256::from(10); // 1.1
        let scaled = ray_div(a, idx).unwrap();
        let back = ray_mul(scaled, idx).unwrap();
        assert!(back == a || back + U256::from(1) == a || a + U256::from(1) == back);
        assert!(ray_div(a, U256::ZERO).is_none());
    }

    #[test]
    fn zero_elapsed_returns_index_unchanged() {
        let idx = RAY * U256::from(2);
        assert_eq!(normalized_income(idx, pct(5), 100, 100).unwrap(), idx);
        assert_eq!(normalized_debt(idx, pct(5), 100, 100).unwrap(), idx);
        assert_eq!(compounded_interest(pct(5), 0).unwrap(), RAY);
    }

    #[test]
    fn linear_one_year_at_5_percent() {
        let got = linear_interest(pct(5), SECONDS_PER_YEAR).unwrap();
        assert_eq!(got, RAY + pct(5));
    }

    fn ray_to_f64(x: U256) -> f64 {
        // good enough for tests: RAY is 1e27, f64 keeps ~15 digits
        let s = x.to_string();
        s.parse::<f64>().unwrap() / 1e27
    }

    #[test]
    fn compounded_one_year_at_5_percent_is_the_three_term_series() {
        // Aave uses ~ 1 + r + r^2/2 + r^3/6 (not the full exponential): about 1.05127
        let got = ray_to_f64(compounded_interest(pct(5), SECONDS_PER_YEAR).unwrap());
        let series = 1.0 + 0.05 + 0.05f64.powi(2) / 2.0 + 0.05f64.powi(3) / 6.0;
        // Aave's integer math truncates the (tiny) per-second cubic term, so allow ~1e-5. This is
        // faithful to the contract: the port must reproduce Aave's rounding, not the ideal value.
        assert!((got - series).abs() < 1e-5, "got {got} series {series}");
        assert!((got - 0.05f64.exp()).abs() < 1e-5, "got {got}");
    }

    #[test]
    fn compounded_is_at_least_linear() {
        for dt in [1u64, 60, 3600, 86_400, 30 * 86_400] {
            let lin = linear_interest(pct(8), dt).unwrap();
            let cmp = compounded_interest(pct(8), dt).unwrap();
            assert!(cmp >= lin, "dt={dt}");
        }
    }

    #[test]
    fn debt_grows_between_events() {
        let idx = RAY; // 1.0
        let scaled = U256::from(1_000_000_000_000_000_000u64); // 1e18 scaled units
        let at_event =
            scaled_to_real(scaled, normalized_debt(idx, pct(10), 1000, 1000).unwrap()).unwrap();
        let later = scaled_to_real(
            scaled,
            normalized_debt(idx, pct(10), 1000, 1000 + SECONDS_PER_YEAR).unwrap(),
        )
        .unwrap();
        assert_eq!(at_event, scaled);
        assert!(later > at_event);
    }
}
