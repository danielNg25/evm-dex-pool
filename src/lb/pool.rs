//! TraderJoe Liquidity Book pool implementation.

use crate::contracts::ILBPair;
use crate::lb::math::*;
use crate::pool::base::{
    EventApplicable, PoolInterface, PoolType, PoolTypeTrait, Topic, TopicList,
};
use alloy::primitives::{Address, U256};
use alloy::rpc::types::Log;
use alloy::sol_types::SolEvent;
use anyhow::{anyhow, Result};
use log::trace;
use serde::{Deserialize, Serialize};
use std::any::Any;
use std::collections::BTreeMap;
use std::fmt;

/// TraderJoe Liquidity Book pool.
///
/// Bin-based AMM where each bin uses constant-sum liquidity: `L = price * x + y`.
/// Swaps iterate through bins from `active_id`, consuming input per bin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LBPool {
    pub address: Address,
    pub token_x: Address,
    pub token_y: Address,
    pub bin_step: u16,
    pub active_id: u32,
    /// Bin ID -> (reserve_x, reserve_y). Only non-empty bins are stored.
    pub bins: BTreeMap<u32, (u128, u128)>,

    // Static fee parameters (from getStaticFeeParameters)
    pub base_factor: u16,
    pub filter_period: u16,
    pub decay_period: u16,
    pub reduction_factor: u16,
    pub variable_fee_control: u32,
    pub protocol_share: u16,
    pub max_volatility_accumulator: u32,

    // Variable fee state (updated from Swap events)
    pub volatility_accumulator: u32,
    #[serde(default)]
    pub volatility_reference: u32,
    #[serde(default)]
    pub id_reference: u32,
    #[serde(default)]
    pub time_of_last_update: u64,

    pub last_updated: u64,
    pub created_at: u64,
}

impl LBPool {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        address: Address,
        token_x: Address,
        token_y: Address,
        bin_step: u16,
        active_id: u32,
        bins: BTreeMap<u32, (u128, u128)>,
        base_factor: u16,
        filter_period: u16,
        decay_period: u16,
        reduction_factor: u16,
        variable_fee_control: u32,
        protocol_share: u16,
        max_volatility_accumulator: u32,
        volatility_accumulator: u32,
        volatility_reference: u32,
        id_reference: u32,
        time_of_last_update: u64,
    ) -> Self {
        let now = chrono::Utc::now().timestamp() as u64;
        Self {
            address,
            token_x,
            token_y,
            bin_step,
            active_id,
            bins,
            base_factor,
            filter_period,
            decay_period,
            reduction_factor,
            variable_fee_control,
            protocol_share,
            max_volatility_accumulator,
            volatility_accumulator,
            volatility_reference,
            id_reference,
            time_of_last_update,
            last_updated: now,
            created_at: now,
        }
    }

    /// Current total fee in 1e18 precision.
    pub fn get_total_fee(&self) -> u128 {
        get_total_fee(
            self.base_factor,
            self.bin_step,
            self.volatility_accumulator,
            self.variable_fee_control,
        )
    }

    /// Fee as an f64 fraction (e.g., 0.003 for 0.3%).
    pub fn fee_f64(&self) -> f64 {
        self.get_total_fee() as f64 / PRECISION as f64
    }

    /// Find the next non-empty bin in the given swap direction.
    ///
    /// - `swap_for_y=true` (sell X for Y): bins contain Y in lower IDs → traverse downward
    /// - `swap_for_y=false` (sell Y for X): bins contain X in higher IDs → traverse upward
    fn next_non_empty_bin(&self, swap_for_y: bool, id: u32) -> Option<u32> {
        if swap_for_y {
            self.bins.range(..id).next_back().map(|(&k, _)| k)
        } else {
            self.bins.range((id + 1)..).next().map(|(&k, _)| k)
        }
    }

    /// Update a bin's reserves. Removes the bin if both reserves are zero.
    pub fn update_bin(&mut self, id: u32, reserve_x: u128, reserve_y: u128) {
        if reserve_x == 0 && reserve_y == 0 {
            self.bins.remove(&id);
        } else {
            self.bins.insert(id, (reserve_x, reserve_y));
        }
    }

    /// Port of `PairParameterHelper.updateReferences()`.
    ///
    /// Applies time-based decay to volatility parameters. Returns `(vol_ref, id_ref)`.
    fn update_references(&self, timestamp: u64) -> (u32, u32) {
        let dt = timestamp.saturating_sub(self.time_of_last_update);
        let mut vol_ref = self.volatility_reference;
        let mut id_ref = self.id_reference;

        if dt >= self.filter_period as u64 {
            // updateIdReference: set idReference = activeId
            id_ref = self.active_id;

            if dt < self.decay_period as u64 {
                // updateVolatilityReference: volRef = volAcc * reductionFactor / 10000
                vol_ref = ((self.volatility_accumulator as u64
                    * self.reduction_factor as u64)
                    / 10_000) as u32;
            } else {
                // Decay period exceeded: reset volatility reference
                vol_ref = 0;
            }
        }

        (vol_ref, id_ref)
    }

    /// Port of `PairParameterHelper.updateVolatilityAccumulator()`.
    ///
    /// Computes the new volatility accumulator for a specific bin id.
    fn compute_volatility_accumulator(&self, id: u32, vol_ref: u32, id_ref: u32) -> u32 {
        let delta_id = id.abs_diff(id_ref);
        let vol_acc = vol_ref as u64 + delta_id as u64 * 10_000;
        vol_acc.min(self.max_volatility_accumulator as u64) as u32
    }

    /// Simulate getSwapOut: given `amount_in` of one token, compute output.
    ///
    /// Uses current wall-clock time for volatility decay. For exact on-chain
    /// comparison at a specific block, use [`simulate_swap_out_at`].
    pub fn simulate_swap_out(
        &self,
        amount_in: u128,
        swap_for_y: bool,
    ) -> Result<(u128, u128, u128)> {
        let timestamp = chrono::Utc::now().timestamp() as u64;
        self.simulate_swap_out_at(amount_in, swap_for_y, timestamp)
    }

    /// Simulate getSwapOut with an explicit timestamp for volatility decay.
    ///
    /// Exact port of `LBPair.getSwapOut()`. Returns `(amount_in_left, amount_out, fee)`.
    /// The `timestamp` should be the block timestamp for exact on-chain comparison.
    pub fn simulate_swap_out_at(
        &self,
        amount_in: u128,
        swap_for_y: bool,
        timestamp: u64,
    ) -> Result<(u128, u128, u128)> {
        let mut amount_in_left = amount_in;
        let mut amount_out: u128 = 0;
        let mut total_fee: u128 = 0;
        let mut id = self.active_id;

        // Step 1: updateReferences(timestamp) — time-based volatility decay
        let (vol_ref, id_ref) = self.update_references(timestamp);

        loop {
            let bin_reserves = self.bins.get(&id);

            if let Some(&(rx, ry)) = bin_reserves {
                let bin_reserve_out = if swap_for_y { ry } else { rx };

                if bin_reserve_out > 0 {
                    // Step 2: updateVolatilityAccumulator(id) — per-bin
                    let vol_acc = self.compute_volatility_accumulator(id, vol_ref, id_ref);
                    let fee = get_total_fee(
                        self.base_factor,
                        self.bin_step,
                        vol_acc,
                        self.variable_fee_control,
                    );

                    let price = get_price_from_id(id, self.bin_step);

                    let max_amount_in = if swap_for_y {
                        shift_div_round_up(U256::from(bin_reserve_out), SCALE_OFFSET, price)
                            .to::<u128>()
                    } else {
                        mul_shift_round_up(U256::from(bin_reserve_out), price, SCALE_OFFSET)
                            .to::<u128>()
                    };

                    let max_fee = get_fee_amount(max_amount_in, fee);
                    let max_amount_in_with_fees = max_amount_in.saturating_add(max_fee);

                    let (amount_in_bin, fee_bin, amount_out_bin);

                    if amount_in_left >= max_amount_in_with_fees {
                        amount_in_bin = max_amount_in_with_fees;
                        fee_bin = max_fee;
                        amount_out_bin = bin_reserve_out;
                    } else {
                        fee_bin = get_fee_amount_from(amount_in_left, fee);
                        let amount_in_no_fee = amount_in_left - fee_bin;
                        amount_in_bin = amount_in_left;

                        amount_out_bin = if swap_for_y {
                            mul_shift_round_down(U256::from(amount_in_no_fee), price, SCALE_OFFSET)
                                .to::<u128>()
                                .min(bin_reserve_out)
                        } else {
                            shift_div_round_down(U256::from(amount_in_no_fee), SCALE_OFFSET, price)
                                .to::<u128>()
                                .min(bin_reserve_out)
                        };
                    }

                    amount_in_left -= amount_in_bin;
                    amount_out += amount_out_bin;
                    total_fee += fee_bin;
                }
            }

            if amount_in_left == 0 {
                break;
            }

            match self.next_non_empty_bin(swap_for_y, id) {
                Some(next_id) => id = next_id,
                None => break,
            }
        }

        Ok((amount_in_left, amount_out, total_fee))
    }

    /// Simulate getSwapIn: given desired `amount_out`, compute required input.
    ///
    /// Uses current wall-clock time for volatility decay.
    pub fn simulate_swap_in(
        &self,
        amount_out: u128,
        swap_for_y: bool,
    ) -> Result<(u128, u128, u128)> {
        let timestamp = chrono::Utc::now().timestamp() as u64;
        self.simulate_swap_in_at(amount_out, swap_for_y, timestamp)
    }

    /// Simulate getSwapIn with an explicit timestamp for volatility decay.
    ///
    /// Exact port of `LBPair.getSwapIn()`. Returns `(amount_in, amount_out_left, fee)`.
    pub fn simulate_swap_in_at(
        &self,
        amount_out: u128,
        swap_for_y: bool,
        timestamp: u64,
    ) -> Result<(u128, u128, u128)> {
        let mut amount_out_left = amount_out;
        let mut amount_in: u128 = 0;
        let mut total_fee: u128 = 0;
        let mut id = self.active_id;

        let (vol_ref, id_ref) = self.update_references(timestamp);

        loop {
            let bin_reserves = self.bins.get(&id);

            if let Some(&(rx, ry)) = bin_reserves {
                let bin_reserve_out = if swap_for_y { ry } else { rx };

                if bin_reserve_out > 0 {
                    let price = get_price_from_id(id, self.bin_step);
                    let amount_out_of_bin = bin_reserve_out.min(amount_out_left);

                    let vol_acc = self.compute_volatility_accumulator(id, vol_ref, id_ref);
                    let fee = get_total_fee(
                        self.base_factor,
                        self.bin_step,
                        vol_acc,
                        self.variable_fee_control,
                    );

                    let amount_in_without_fee = if swap_for_y {
                        shift_div_round_up(U256::from(amount_out_of_bin), SCALE_OFFSET, price)
                            .to::<u128>()
                    } else {
                        mul_shift_round_up(U256::from(amount_out_of_bin), price, SCALE_OFFSET)
                            .to::<u128>()
                    };

                    let fee_amount = get_fee_amount(amount_in_without_fee, fee);

                    amount_in += amount_in_without_fee + fee_amount;
                    amount_out_left -= amount_out_of_bin;
                    total_fee += fee_amount;
                }
            }

            if amount_out_left == 0 {
                break;
            }

            match self.next_non_empty_bin(swap_for_y, id) {
                Some(next_id) => id = next_id,
                None => break,
            }
        }

        Ok((amount_in, amount_out_left, total_fee))
    }
}

// ─── PoolInterface ───────────────────────────────────────────────────────────

impl PoolInterface for LBPool {
    fn calculate_output(&self, token_in: &Address, amount_in: U256) -> Result<U256> {
        let swap_for_y = if token_in == &self.token_x {
            true
        } else if token_in == &self.token_y {
            false
        } else {
            return Err(anyhow!(
                "Token {} not in LB pool {}",
                token_in,
                self.address
            ));
        };

        let amount_in_128: u128 = amount_in
            .try_into()
            .map_err(|_| anyhow!("Amount too large for LB pool (exceeds u128)"))?;

        let (amount_in_left, amount_out, _fee) =
            self.simulate_swap_out(amount_in_128, swap_for_y)?;
        if amount_in_left > 0 {
            return Err(anyhow!(
                "Insufficient liquidity in LB pool: {} of {} input remaining",
                amount_in_left,
                amount_in_128
            ));
        }
        Ok(U256::from(amount_out))
    }

    fn calculate_input(&self, token_out: &Address, amount_out: U256) -> Result<U256> {
        let swap_for_y = if token_out == &self.token_y {
            true
        } else if token_out == &self.token_x {
            false
        } else {
            return Err(anyhow!(
                "Token {} not in LB pool {}",
                token_out,
                self.address
            ));
        };

        let amount_out_128: u128 = amount_out
            .try_into()
            .map_err(|_| anyhow!("Amount too large for LB pool (exceeds u128)"))?;

        let (amount_in, amount_out_left, _fee) =
            self.simulate_swap_in(amount_out_128, swap_for_y)?;
        if amount_out_left > 0 {
            return Err(anyhow!(
                "Insufficient liquidity in LB pool: {} of {} output remaining",
                amount_out_left,
                amount_out_128
            ));
        }
        Ok(U256::from(amount_in))
    }

    fn apply_swap(
        &mut self,
        _token_in: &Address,
        _amount_in: U256,
        _amount_out: U256,
    ) -> Result<()> {
        // State is updated via apply_log from Swap events
        self.last_updated = chrono::Utc::now().timestamp() as u64;
        Ok(())
    }

    fn address(&self) -> Address {
        self.address
    }

    fn tokens(&self) -> (Address, Address) {
        (self.token_x, self.token_y)
    }

    fn fee(&self) -> f64 {
        self.fee_f64()
    }

    fn fee_raw(&self) -> u64 {
        // Return base fee in a comparable format to V2/V3
        // Using 1_000_000 basis like V2 (fee_raw / 1_000_000 = fee fraction)
        let fee_1e18 = self.get_total_fee();
        // Convert from 1e18 to 1_000_000 basis: fee * 1_000_000 / 1e18
        (fee_1e18 / 1_000_000_000_000) as u64
    }

    fn id(&self) -> String {
        format!("lb-{:?}-{}", self.address, self.bin_step)
    }

    fn contains_token(&self, token: &Address) -> bool {
        *token == self.token_x || *token == self.token_y
    }

    fn clone_box(&self) -> Box<dyn PoolInterface + Send + Sync> {
        Box::new(self.clone())
    }

    fn log_summary(&self) -> String {
        format!(
            "LB Pool {} ({} <> {}, binStep={}, activeId={}, bins={})",
            self.address,
            self.token_x,
            self.token_y,
            self.bin_step,
            self.active_id,
            self.bins.len(),
        )
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

// ─── EventApplicable ─────────────────────────────────────────────────────────

impl EventApplicable for LBPool {
    fn apply_log(&mut self, event: &Log) -> Result<()> {
        match event.topic0() {
            Some(&ILBPair::Swap::SIGNATURE_HASH) => {
                let swap_data: ILBPair::Swap = event.log_decode()?.inner.data;
                let id: u32 = swap_data.id.to();

                // Decode packed amounts
                let (in_x, in_y) = decode_amounts(swap_data.amountsIn);
                let (out_x, out_y) = decode_amounts(swap_data.amountsOut);

                // The Swap event's amountsIn is what was added to the bin (net of protocol fees).
                // amountsOut is what was removed from the bin.
                // bin[id] = bin[id] + amountsIn - amountsOut
                let (rx, ry) = self.bins.get(&id).copied().unwrap_or((0, 0));
                let new_rx = rx.saturating_add(in_x).saturating_sub(out_x);
                let new_ry = ry.saturating_add(in_y).saturating_sub(out_y);
                self.update_bin(id, new_rx, new_ry);

                // Update active_id to the bin where this swap event occurred.
                // For multi-bin swaps, the last Swap event carries the final active ID.
                self.active_id = id;

                // Update volatility accumulator from the event
                self.volatility_accumulator = swap_data.volatilityAccumulator.to();

                // Update id_reference and time_of_last_update to reflect
                // the post-swap state. The contract calls updateReferences()
                // before the swap loop, so after the swap completes these
                // values are current.
                self.id_reference = id;
                let now = chrono::Utc::now().timestamp() as u64;
                self.time_of_last_update = now;

                self.last_updated = now;
                Ok(())
            }
            Some(&ILBPair::DepositedToBins::SIGNATURE_HASH) => {
                let data: ILBPair::DepositedToBins = event.log_decode()?.inner.data;
                for (i, id_u256) in data.ids.iter().enumerate() {
                    if let Some(amounts_bytes) = data.amounts.get(i) {
                        let id: u32 = (*id_u256).try_into().unwrap_or(u32::MAX);
                        let (add_x, add_y) = decode_amounts(*amounts_bytes);
                        let (rx, ry) = self.bins.get(&id).copied().unwrap_or((0, 0));
                        self.update_bin(id, rx.saturating_add(add_x), ry.saturating_add(add_y));
                    }
                }
                self.last_updated = chrono::Utc::now().timestamp() as u64;
                Ok(())
            }
            Some(&ILBPair::WithdrawnFromBins::SIGNATURE_HASH) => {
                let data: ILBPair::WithdrawnFromBins = event.log_decode()?.inner.data;
                for (i, id_u256) in data.ids.iter().enumerate() {
                    if let Some(amounts_bytes) = data.amounts.get(i) {
                        let id: u32 = (*id_u256).try_into().unwrap_or(u32::MAX);
                        let (sub_x, sub_y) = decode_amounts(*amounts_bytes);
                        let (rx, ry) = self.bins.get(&id).copied().unwrap_or((0, 0));
                        self.update_bin(id, rx.saturating_sub(sub_x), ry.saturating_sub(sub_y));
                    }
                }
                self.last_updated = chrono::Utc::now().timestamp() as u64;
                Ok(())
            }
            Some(&ILBPair::StaticFeeParametersSet::SIGNATURE_HASH) => {
                let data: ILBPair::StaticFeeParametersSet = event.log_decode()?.inner.data;
                self.base_factor = data.baseFactor;
                self.filter_period = data.filterPeriod;
                self.decay_period = data.decayPeriod;
                self.reduction_factor = data.reductionFactor;
                self.variable_fee_control = data.variableFeeControl.to();
                self.protocol_share = data.protocolShare;
                self.max_volatility_accumulator = data.maxVolatilityAccumulator.to();
                Ok(())
            }
            _ => {
                trace!("Ignoring unknown event for LB pool {}", self.address);
                Ok(())
            }
        }
    }
}

// ─── TopicList ───────────────────────────────────────────────────────────────

impl TopicList for LBPool {
    fn topics() -> Vec<Topic> {
        vec![
            ILBPair::Swap::SIGNATURE_HASH,
            ILBPair::DepositedToBins::SIGNATURE_HASH,
            ILBPair::WithdrawnFromBins::SIGNATURE_HASH,
            ILBPair::StaticFeeParametersSet::SIGNATURE_HASH,
        ]
    }

    fn profitable_topics() -> Vec<Topic> {
        vec![ILBPair::Swap::SIGNATURE_HASH]
    }
}

// ─── PoolTypeTrait ───────────────────────────────────────────────────────────

impl PoolTypeTrait for LBPool {
    fn pool_type(&self) -> PoolType {
        PoolType::TraderJoeLB
    }
}

// ─── Display ─────────────────────────────────────────────────────────────────

impl fmt::Display for LBPool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "LBPool({}, {}<>{}, binStep={}, activeId={}, bins={})",
            self.address,
            self.token_x,
            self.token_y,
            self.bin_step,
            self.active_id,
            self.bins.len(),
        )
    }
}
