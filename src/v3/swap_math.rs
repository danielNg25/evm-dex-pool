use alloy::primitives::{aliases::U24, Uint, I256, U160, U256};
use anyhow::{anyhow, Result};

use crate::v3::{
    get_amount_0_delta, get_amount_1_delta, get_next_sqrt_price_from_input,
    get_next_sqrt_price_from_output, mul_div, mul_div_rounding_up, MIN_TICK_I32,
};

use super::{
    add_delta, get_sqrt_ratio_at_tick, TickDataProvider, TickIndex, TickMap, TickMath,
    MAX_SQRT_RATIO, MAX_TICK_I32, MIN_SQRT_RATIO, ONE,
};

#[derive(Clone, Copy, Debug, Default)]
pub struct SwapState<I = i32> {
    pub amount_specified_remaining: I256,
    pub amount_calculated: I256,
    pub sqrt_price_x96: U160,
    pub tick_current: I,
    pub liquidity: u128,
}

#[derive(Clone, Copy, Debug, Default)]
struct StepComputations<I = i32> {
    sqrt_price_start_x96: U160,
    tick_next: I,
    initialized: bool,
    sqrt_price_next_x96: U160,
    amount_in: U256,
    amount_out: U256,
    fee_amount: U256,
}

/// Computes the result of swapping some amount in, or amount out, given the parameters of the swap
#[inline]
pub fn compute_swap_step<const BITS: usize, const LIMBS: usize>(
    sqrt_ratio_current_x96: Uint<BITS, LIMBS>,
    sqrt_ratio_target_x96: Uint<BITS, LIMBS>,
    liquidity: u128,
    amount_remaining: I256,
    fee_pips: U24,
) -> Result<(Uint<BITS, LIMBS>, U256, U256, U256)> {
    const MAX_FEE: U256 = U256::from_limbs([1000000, 0, 0, 0]);
    let fee_pips = U256::from(fee_pips);
    let fee_complement = MAX_FEE - fee_pips;
    let zero_for_one = sqrt_ratio_current_x96 >= sqrt_ratio_target_x96;
    let exact_in = amount_remaining >= I256::ZERO;

    let sqrt_ratio_next_x96: Uint<BITS, LIMBS>;
    let mut amount_in: U256;
    let mut amount_out: U256;
    let fee_amount: U256;
    if exact_in {
        let amount_remaining_abs = amount_remaining.into_raw();
        let amount_remaining_less_fee = mul_div(amount_remaining_abs, fee_complement, MAX_FEE)?;

        amount_in = if zero_for_one {
            get_amount_0_delta(
                sqrt_ratio_target_x96,
                sqrt_ratio_current_x96,
                liquidity,
                true,
            )?
        } else {
            get_amount_1_delta(
                sqrt_ratio_current_x96,
                sqrt_ratio_target_x96,
                liquidity,
                true,
            )?
        };

        if amount_remaining_less_fee >= amount_in {
            sqrt_ratio_next_x96 = sqrt_ratio_target_x96;
            fee_amount = mul_div_rounding_up(amount_in, fee_pips, fee_complement)?;
        } else {
            amount_in = amount_remaining_less_fee;
            sqrt_ratio_next_x96 = get_next_sqrt_price_from_input(
                sqrt_ratio_current_x96,
                liquidity,
                amount_in,
                zero_for_one,
            )?;
            fee_amount = amount_remaining_abs - amount_in;
        }

        amount_out = if zero_for_one {
            get_amount_1_delta(
                sqrt_ratio_next_x96,
                sqrt_ratio_current_x96,
                liquidity,
                false,
            )?
        } else {
            get_amount_0_delta(
                sqrt_ratio_current_x96,
                sqrt_ratio_next_x96,
                liquidity,
                false,
            )?
        };
    } else {
        let amount_remaining_abs = (-amount_remaining).into_raw();

        amount_out = if zero_for_one {
            get_amount_1_delta(
                sqrt_ratio_target_x96,
                sqrt_ratio_current_x96,
                liquidity,
                false,
            )?
        } else {
            get_amount_0_delta(
                sqrt_ratio_current_x96,
                sqrt_ratio_target_x96,
                liquidity,
                false,
            )?
        };

        if amount_remaining_abs >= amount_out {
            sqrt_ratio_next_x96 = sqrt_ratio_target_x96;
        } else {
            amount_out = amount_remaining_abs;
            sqrt_ratio_next_x96 = get_next_sqrt_price_from_output(
                sqrt_ratio_current_x96,
                liquidity,
                amount_out,
                zero_for_one,
            )?;
        }

        amount_in = if zero_for_one {
            get_amount_0_delta(sqrt_ratio_next_x96, sqrt_ratio_current_x96, liquidity, true)?
        } else {
            get_amount_1_delta(sqrt_ratio_current_x96, sqrt_ratio_next_x96, liquidity, true)?
        };
        fee_amount = mul_div_rounding_up(amount_in, fee_pips, fee_complement)?;
    }

    Ok((sqrt_ratio_next_x96, amount_in, amount_out, fee_amount))
}

#[inline]
#[allow(clippy::too_many_arguments)]
pub fn v3_swap(
    fee: U24,
    sqrt_price_x96: U160,
    tick_current: i32,
    liquidity: u128,
    tick_data_provider: &TickMap,
    zero_for_one: bool,
    amount_specified: I256,
    sqrt_price_limit_x96: Option<U160>,
) -> Result<SwapState<i32>> {
    let sqrt_price_limit_x96 = sqrt_price_limit_x96.unwrap_or(if zero_for_one {
        MIN_SQRT_RATIO + ONE
    } else {
        MAX_SQRT_RATIO - ONE
    });

    if zero_for_one {
        if !(sqrt_price_limit_x96 > MIN_SQRT_RATIO) {
            return Err(anyhow!("RATIO_MIN"));
        }
        if !(sqrt_price_limit_x96 < sqrt_price_x96) {
            return Err(anyhow!("RATIO_CURRENT"));
        }
    } else {
        if !(sqrt_price_limit_x96 < MAX_SQRT_RATIO) {
            return Err(anyhow!("RATIO_MAX"));
        }
        if !(sqrt_price_limit_x96 > sqrt_price_x96) {
            return Err(anyhow!("RATIO_CURRENT"));
        }
    }

    let exact_input = amount_specified >= I256::ZERO;

    let mut state = SwapState {
        amount_specified_remaining: amount_specified,
        amount_calculated: I256::ZERO,
        sqrt_price_x96,
        tick_current,
        liquidity,
    };

    while !state.amount_specified_remaining.is_zero()
        && state.sqrt_price_x96 != sqrt_price_limit_x96
    {
        let mut step = StepComputations {
            sqrt_price_start_x96: state.sqrt_price_x96,
            ..Default::default()
        };

        (step.tick_next, step.initialized) = tick_data_provider
            .next_initialized_tick_within_one_word(state.tick_current, zero_for_one)?;
        step.tick_next = step.tick_next.clamp(MIN_TICK_I32, MAX_TICK_I32);
        step.sqrt_price_next_x96 = get_sqrt_ratio_at_tick(step.tick_next.to_i24())?;

        (
            state.sqrt_price_x96,
            step.amount_in,
            step.amount_out,
            step.fee_amount,
        ) = compute_swap_step(
            state.sqrt_price_x96,
            if zero_for_one {
                step.sqrt_price_next_x96.max(sqrt_price_limit_x96)
            } else {
                step.sqrt_price_next_x96.min(sqrt_price_limit_x96)
            },
            state.liquidity,
            state.amount_specified_remaining,
            fee,
        )?;

        if exact_input {
            state.amount_specified_remaining = I256::from_raw(
                state.amount_specified_remaining.into_raw() - step.amount_in - step.fee_amount,
            );
            state.amount_calculated =
                I256::from_raw(state.amount_calculated.into_raw() - step.amount_out);
        } else {
            state.amount_specified_remaining =
                I256::from_raw(state.amount_specified_remaining.into_raw() + step.amount_out);
            state.amount_calculated = I256::from_raw(
                state.amount_calculated.into_raw() + step.amount_in + step.fee_amount,
            );
        }

        if state.sqrt_price_x96 == step.sqrt_price_next_x96 {
            if step.initialized {
                let mut liquidity_net = tick_data_provider.get_tick(step.tick_next)?.liquidity_net;
                if zero_for_one {
                    liquidity_net = -liquidity_net;
                }
                state.liquidity = add_delta(state.liquidity, liquidity_net)?;
            }
            state.tick_current = if zero_for_one {
                step.tick_next - i32::ONE
            } else {
                step.tick_next
            };

            if state.liquidity == 0 {
                break;
            }
        } else if state.sqrt_price_x96 != step.sqrt_price_start_x96 {
            state.tick_current =
                TickIndex::from_i24(state.sqrt_price_x96.get_tick_at_sqrt_ratio()?);
        }
    }

    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::U160;

    #[test]
    fn test_compute_swap_step() {
        let amount_specified_remaining = I256::from_raw(U256::from_limbs([
            18446744073709540431,
            18446744073709551615,
            18446744073709551615,
            18446744073709551615,
        ]));
        let (sqrt_price_next_x96, amount_in, amount_out, fee_amount) = compute_swap_step(
            U160::from_limbs([7164297123421688246, 4074563739, 0]),
            U160::from_limbs([7829751401545787782, 4282102344, 0]),
            94868,
            amount_specified_remaining,
            U24::from(3000),
        )
        .unwrap();
        assert_eq!(
            sqrt_price_next_x96,
            U160::from_limbs([7829751401545787782, 4282102344, 0])
        );
        assert_eq!(amount_in, U256::from_limbs([4585, 0, 0, 0]));
        assert_eq!(amount_out, U256::from_limbs([4846, 0, 0, 0]));
        assert_eq!(fee_amount, U256::from_limbs([14, 0, 0, 0]));
    }
}
