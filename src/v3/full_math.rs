use super::{Q96, THREE, TWO};
use alloy::primitives::{uint, Uint, U256};
use anyhow::{anyhow, Result};

const ONE: U256 = uint!(1_U256);

/// Full precision arithmetic operations for [`Uint`] types.
pub trait FullMath {
    fn mul_div(self, b: U256, denominator: U256) -> Result<U256>;
    fn mul_div_rounding_up(self, b: U256, denominator: U256) -> Result<U256>;
    fn mul_div_q96(self, b: U256) -> Result<U256>;
}

impl<const BITS: usize, const LIMBS: usize> FullMath for Uint<BITS, LIMBS> {
    #[inline]
    fn mul_div(self, b: U256, denominator: U256) -> Result<U256> {
        mul_div(U256::from(self), b, denominator)
    }

    #[inline]
    fn mul_div_rounding_up(self, b: U256, denominator: U256) -> Result<U256> {
        mul_div_rounding_up(U256::from(self), b, denominator)
    }

    #[inline]
    fn mul_div_q96(self, b: U256) -> Result<U256> {
        mul_div_q96(U256::from(self), b)
    }
}

/// Calculates floor(a*b/denominator) with full precision. Throws if result overflows a uint256 or
/// denominator == 0
#[inline]
pub fn mul_div(a: U256, b: U256, mut denominator: U256) -> Result<U256> {
    let mm = a.mul_mod(b, U256::MAX);
    let mut prod_0 = a * b;
    let mut prod_1 = mm - prod_0 - U256::from_limbs([(mm < prod_0) as u64, 0, 0, 0]);

    if denominator <= prod_1 {
        return Err(anyhow!("MulDiv overflow"));
    }

    if prod_1.is_zero() {
        return Ok(prod_0 / denominator);
    }

    let remainder = a.mul_mod(b, denominator);
    prod_1 -= U256::from_limbs([(remainder > prod_0) as u64, 0, 0, 0]);
    prod_0 -= remainder;

    let mut twos = (-denominator) & denominator;
    denominator /= twos;
    prod_0 /= twos;
    twos = (-twos) / twos + ONE;
    prod_0 |= prod_1 * twos;

    let mut inv = (THREE * denominator) ^ TWO;
    inv *= TWO - denominator * inv; // inverse mod 2**8
    inv *= TWO - denominator * inv; // inverse mod 2**16
    inv *= TWO - denominator * inv; // inverse mod 2**32
    inv *= TWO - denominator * inv; // inverse mod 2**64
    inv *= TWO - denominator * inv; // inverse mod 2**128
    inv *= TWO - denominator * inv; // inverse mod 2**256

    Ok(prod_0 * inv)
}

/// Calculates ceil(a*b/denominator) with full precision. Throws if result overflows a uint256 or
/// denominator == 0
#[inline]
pub fn mul_div_rounding_up(a: U256, b: U256, denominator: U256) -> Result<U256> {
    let result = mul_div(a, b, denominator)?;

    if a.mul_mod(b, denominator).is_zero() {
        Ok(result)
    } else if result == U256::MAX {
        Err(anyhow!("MulDiv overflow"))
    } else {
        Ok(result + ONE)
    }
}

/// Calculates a * b / 2^96 with full precision.
#[inline]
pub fn mul_div_q96(a: U256, b: U256) -> Result<U256> {
    let prod0 = a * b;
    let mm = a.mul_mod(b, U256::MAX);
    let prod1 = mm - prod0 - U256::from_limbs([(mm < prod0) as u64, 0, 0, 0]);
    if prod1 >= Q96 {
        return Err(anyhow!("MulDiv overflow"));
    }
    Ok((prod0 >> 96) | (prod1 << 160))
}
