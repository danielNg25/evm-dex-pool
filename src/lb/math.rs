//! TraderJoe Liquidity Book math library.
//!
//! Ports 128.128 fixed-point arithmetic from the Solidity LB v2 contracts:
//! - `Uint128x128Math.pow()` — binary exponentiation
//! - `PriceHelper.getPriceFromId()` / `getBase()` — bin price calculations
//! - `FeeHelper` — fee calculations
//! - `PackedUint128Math.decode()` — bytes32 unpacking

use alloy::primitives::{FixedBytes, Uint, U256};

/// 512-bit unsigned integer for intermediate calculations.
type U512 = Uint<512, 8>;

// ─── Constants ───────────────────────────────────────────────────────────────

/// 128.128 fixed-point scale offset
pub const SCALE_OFFSET: u32 = 128;

/// 1 << 128 (the "1.0" in 128.128 fixed-point)
pub const SCALE: U256 = U256::from_limbs([0, 0, 1, 0]);

/// 1e18 — precision for fee calculations
pub const PRECISION: u128 = 1_000_000_000_000_000_000;

/// 1e36 — squared precision
pub const SQUARED_PRECISION: u128 = 0; // not needed directly, use U256 ops

/// 10% max fee (0.1e18)
pub const MAX_FEE: u128 = 100_000_000_000_000_000;

/// 10_000 basis points = 100%
pub const BASIS_POINT_MAX: u128 = 10_000;

/// Bin ID where price = 1.0 (2^23)
pub const REAL_ID_SHIFT: i32 = 1 << 23; // 8_388_608

// ─── Price Math ──────────────────────────────────────────────────────────────

/// Calculate base = 1 + binStep/10000 in 128.128 fixed-point.
///
/// Port of `PriceHelper.getBase()`:
/// `SCALE + (uint256(binStep) << SCALE_OFFSET) / BASIS_POINT_MAX`
pub fn get_base(bin_step: u16) -> U256 {
    SCALE + (U256::from(bin_step) << SCALE_OFFSET) / U256::from(BASIS_POINT_MAX)
}

/// Calculate exponent = id - REAL_ID_SHIFT
pub fn get_exponent(id: u32) -> i32 {
    id as i32 - REAL_ID_SHIFT
}

/// Calculate price from bin ID and bin step in 128.128 fixed-point.
///
/// Port of `PriceHelper.getPriceFromId()`:
/// `getBase(binStep).pow(getExponent(id))`
pub fn get_price_from_id(id: u32, bin_step: u16) -> U256 {
    let base = get_base(bin_step);
    let exponent = get_exponent(id);
    pow_128x128(base, exponent)
}

/// 128.128 fixed-point exponentiation via binary exponentiation.
///
/// Port of `Uint128x128Math.pow()`. Computes `x^y` where x is a 128.128
/// fixed-point number and y is a signed integer. Uses 20-bit binary
/// exponentiation with intermediate 512-bit products.
///
/// For negative y, computes `1 / x^|y|`.
pub fn pow_128x128(x: U256, y: i32) -> U256 {
    if y == 0 {
        return SCALE;
    }

    let abs_y = y.unsigned_abs();
    let mut invert = y < 0;

    // If x > 2^128, work with 1/x and flip inversion
    let mut squared = x;
    if squared > SCALE {
        squared = U256::MAX / squared;
        invert = !invert;
    }

    let mut result = SCALE;

    // Binary exponentiation: iterate over 20 bits
    for bit in 0..20u32 {
        if abs_y & (1 << bit) != 0 {
            result = mul_128x128(result, squared);
        }
        squared = mul_128x128(squared, squared);
    }

    if result.is_zero() {
        // Underflow
        return U256::ZERO;
    }

    if invert {
        U256::MAX / result
    } else {
        result
    }
}

/// Multiply two 128.128 fixed-point numbers: (a * b) >> 128.
///
/// Uses 512-bit intermediate to avoid overflow.
fn mul_128x128(a: U256, b: U256) -> U256 {
    let product: U512 = a.widening_mul(b);
    let shifted = product >> SCALE_OFFSET;
    let limbs = shifted.as_limbs();
    U256::from_limbs([limbs[0], limbs[1], limbs[2], limbs[3]])
}

// ─── Fee Math ────────────────────────────────────────────────────────────────

/// Calculate base fee in 1e18 precision.
///
/// Port of `PairParameterHelper.getBaseFee()`:
/// `baseFactor * binStep * 1e10`
pub fn get_base_fee(base_factor: u16, bin_step: u16) -> u128 {
    base_factor as u128 * bin_step as u128 * 10_000_000_000u128
}

/// Calculate variable fee in 1e18 precision.
///
/// Port of `PairParameterHelper.getVariableFee()`:
/// `(volatilityAccumulator * binStep)^2 * variableFeeControl / 100`
pub fn get_variable_fee(
    volatility_accumulator: u32,
    bin_step: u16,
    variable_fee_control: u32,
) -> u128 {
    if variable_fee_control == 0 {
        return 0;
    }
    let prod = volatility_accumulator as u128 * bin_step as u128;
    // prod * prod * vfc / 100
    // Use u128 arithmetic — max prod ~ 1M * 100 = 100M, prod^2 ~ 1e16, * vfc ~ 1e22
    // This fits in u128
    (prod * prod * variable_fee_control as u128 + 99) / 100
}

/// Calculate total fee (base + variable), capped at MAX_FEE.
///
/// Port of `PairParameterHelper.getTotalFee()`
pub fn get_total_fee(
    base_factor: u16,
    bin_step: u16,
    volatility_accumulator: u32,
    variable_fee_control: u32,
) -> u128 {
    let total = get_base_fee(base_factor, bin_step)
        + get_variable_fee(volatility_accumulator, bin_step, variable_fee_control);
    total.min(MAX_FEE)
}

/// Calculate fee amount from an amount that already includes fees.
///
/// Port of `FeeHelper.getFeeAmountFrom()`:
/// `(amountWithFees * totalFee + PRECISION - 1) / PRECISION`
///
/// Rounds up.
pub fn get_fee_amount_from(amount_with_fees: u128, total_fee: u128) -> u128 {
    debug_assert!(total_fee <= MAX_FEE, "Fee too large");
    // Use U256 to avoid u128 overflow: max amount * max fee ~ 2^128 * 1e17
    let numerator =
        U256::from(amount_with_fees) * U256::from(total_fee) + U256::from(PRECISION - 1);
    let result = numerator / U256::from(PRECISION);
    result.to::<u128>()
}

/// Calculate fee amount to add to a net amount.
///
/// Port of `FeeHelper.getFeeAmount()`:
/// `(amount * totalFee + (PRECISION - totalFee - 1)) / (PRECISION - totalFee)`
///
/// Rounds up.
pub fn get_fee_amount(amount: u128, total_fee: u128) -> u128 {
    debug_assert!(total_fee <= MAX_FEE, "Fee too large");
    let denominator = PRECISION - total_fee;
    let numerator = U256::from(amount) * U256::from(total_fee) + U256::from(denominator - 1);
    let result = numerator / U256::from(denominator);
    result.to::<u128>()
}

// ─── Packed Amount Helpers ───────────────────────────────────────────────────

/// Decode a packed bytes32 into (amount_x, amount_y).
///
/// Port of `PackedUint128Math.decode()`:
/// Low 128 bits = X, high 128 bits = Y.
pub fn decode_amounts(packed: FixedBytes<32>) -> (u128, u128) {
    let bytes = packed.as_slice();
    // FixedBytes is big-endian. In Solidity, low 128 bits = X.
    // bytes[0..16] = high 128 bits (Y), bytes[16..32] = low 128 bits (X)
    let y = u128::from_be_bytes(bytes[0..16].try_into().unwrap());
    let x = u128::from_be_bytes(bytes[16..32].try_into().unwrap());
    (x, y)
}

/// Decode a specific side from packed bytes32.
/// If `decode_x` is true, returns the low 128 bits (X); otherwise the high 128 bits (Y).
pub fn decode_amount(packed: FixedBytes<32>, decode_x: bool) -> u128 {
    let (x, y) = decode_amounts(packed);
    if decode_x {
        x
    } else {
        y
    }
}

// ─── Swap Amount Helpers ─────────────────────────────────────────────────────

/// Multiply then shift right, rounding down: `(x * y) >> offset`
///
/// Port of `Uint256x256Math.mulShiftRoundDown()`
pub fn mul_shift_round_down(x: U256, y: U256, offset: u32) -> U256 {
    let product: U512 = x.widening_mul(y);
    let shifted = product >> offset;
    let limbs = shifted.as_limbs();
    U256::from_limbs([limbs[0], limbs[1], limbs[2], limbs[3]])
}

/// Multiply then shift right, rounding up: `ceil((x * y) >> offset)`
///
/// Port of `Uint256x256Math.mulShiftRoundUp()`
pub fn mul_shift_round_up(x: U256, y: U256, offset: u32) -> U256 {
    let result = mul_shift_round_down(x, y, offset);
    let product: U512 = x.widening_mul(y);
    let mask = if offset >= 512 {
        U512::ZERO
    } else {
        (U512::from(1u64) << offset) - U512::from(1u64)
    };
    if (product & mask) > U512::ZERO {
        result + U256::from(1u64)
    } else {
        result
    }
}

/// Shift left then divide, rounding down: `(x << offset) / y`
///
/// Port of `Uint256x256Math.shiftDivRoundDown()`
pub fn shift_div_round_down(x: U256, offset: u32, denominator: U256) -> U256 {
    if denominator.is_zero() {
        return U256::ZERO;
    }
    let x_wide = U512::from(x);
    let shifted = x_wide << offset;
    let denom_wide = U512::from(denominator);
    let result = shifted / denom_wide;
    let limbs = result.as_limbs();
    U256::from_limbs([limbs[0], limbs[1], limbs[2], limbs[3]])
}

/// Shift left then divide, rounding up: `ceil((x << offset) / y)`
///
/// Port of `Uint256x256Math.shiftDivRoundUp()`
pub fn shift_div_round_up(x: U256, offset: u32, denominator: U256) -> U256 {
    if denominator.is_zero() {
        return U256::ZERO;
    }
    let result = shift_div_round_down(x, offset, denominator);
    let x_wide = U512::from(x);
    let shifted = x_wide << offset;
    let denom_wide = U512::from(denominator);
    if shifted % denom_wide > U512::ZERO {
        result + U256::from(1u64)
    } else {
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scale_constant() {
        assert_eq!(SCALE, U256::from(1u64) << 128);
    }

    #[test]
    fn test_get_base() {
        // binStep = 1 (0.01%): base = 1 + 1/10000 in 128.128
        let base = get_base(1);
        // Should be slightly above SCALE
        assert!(base > SCALE);
        // base - SCALE = SCALE / 10000
        let diff = base - SCALE;
        let expected_diff = SCALE / U256::from(10000u64);
        assert_eq!(diff, expected_diff);
    }

    #[test]
    fn test_get_base_25() {
        // binStep = 25 (0.25%): base = 1 + 25/10000 = 1.0025
        let base = get_base(25);
        let diff = base - SCALE;
        let expected_diff = U256::from(25u64) * SCALE / U256::from(10000u64);
        assert_eq!(diff, expected_diff);
    }

    #[test]
    fn test_pow_identity() {
        // Any base^0 = SCALE (1.0 in 128.128)
        assert_eq!(pow_128x128(get_base(25), 0), SCALE);
    }

    #[test]
    fn test_pow_one() {
        // base^1 = base
        let base = get_base(25);
        let result = pow_128x128(base, 1);
        assert_eq!(result, base);
    }

    #[test]
    fn test_pow_negative_one() {
        // base^(-1) = 1/base
        let base = get_base(25);
        let result = pow_128x128(base, -1);
        let expected = U256::MAX / base;
        // Allow 1 unit of rounding error
        let diff = if result > expected {
            result - expected
        } else {
            expected - result
        };
        assert!(diff <= U256::from(1u64));
    }

    #[test]
    fn test_price_at_center_bin() {
        // At bin_id = 2^23 (REAL_ID_SHIFT), exponent = 0, price = 1.0 = SCALE
        let price = get_price_from_id(REAL_ID_SHIFT as u32, 25);
        assert_eq!(price, SCALE);
    }

    #[test]
    fn test_price_monotonic() {
        // Higher bin ID → higher price
        let center = REAL_ID_SHIFT as u32;
        let p1 = get_price_from_id(center, 25);
        let p2 = get_price_from_id(center + 1, 25);
        let p3 = get_price_from_id(center + 10, 25);
        assert!(p2 > p1);
        assert!(p3 > p2);

        let p0 = get_price_from_id(center - 1, 25);
        assert!(p0 < p1);
    }

    #[test]
    fn test_base_fee() {
        // baseFactor=15, binStep=25: 15 * 25 * 1e10 = 3.75e12
        let fee = get_base_fee(15, 25);
        assert_eq!(fee, 3_750_000_000_000u128);
    }

    #[test]
    fn test_variable_fee_zero_control() {
        assert_eq!(get_variable_fee(1000, 25, 0), 0);
    }

    #[test]
    fn test_total_fee_capped() {
        // Very large parameters should be capped at MAX_FEE
        let fee = get_total_fee(u16::MAX, u16::MAX, u32::MAX, u32::MAX);
        assert!(fee <= MAX_FEE);
    }

    #[test]
    fn test_fee_amount_from() {
        // If totalFee = 0.003e18 (0.3%), and amount = 1000e18 with fees,
        // fee should be ~3e18
        let total_fee = 3_000_000_000_000_000u128; // 0.3%
        let amount_with_fees = 1_000_000_000_000_000_000_000u128; // 1000e18
        let fee = get_fee_amount_from(amount_with_fees, total_fee);
        // fee = (1000e18 * 0.003e18 + 1e18 - 1) / 1e18 = 3e18
        assert_eq!(fee, 3_000_000_000_000_000_000u128);
    }

    #[test]
    fn test_decode_amounts() {
        // Construct a packed bytes32: X=100, Y=200
        let mut bytes = [0u8; 32];
        // Y in high 128 bits (big-endian bytes 0..16)
        bytes[0..16].copy_from_slice(&200u128.to_be_bytes());
        // X in low 128 bits (big-endian bytes 16..32)
        bytes[16..32].copy_from_slice(&100u128.to_be_bytes());
        let packed = FixedBytes::<32>::from(bytes);
        let (x, y) = decode_amounts(packed);
        assert_eq!(x, 100);
        assert_eq!(y, 200);
    }

    #[test]
    fn test_mul_shift_round_down() {
        // Simple test: (SCALE * SCALE) >> 128 = SCALE (1.0 * 1.0 = 1.0)
        let result = mul_shift_round_down(SCALE, SCALE, SCALE_OFFSET);
        assert_eq!(result, SCALE);
    }

    #[test]
    fn test_shift_div_round_down() {
        // (100 << 128) / SCALE = 100
        let result = shift_div_round_down(U256::from(100u64), SCALE_OFFSET, SCALE);
        assert_eq!(result, U256::from(100u64));
    }
}
