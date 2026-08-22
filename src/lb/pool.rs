//! TraderJoe Liquidity Book pool implementation.

use crate::contracts::{ILBPair, ILBPairV20};
use crate::lb::math::*;
use crate::lb::version::LBVersion;
use crate::pool::base::{
    EventApplicable, PoolInterface, PoolType, PoolTypeTrait, QuoteContext, Topic, TopicList,
};
use alloy::primitives::{Address, B256, U256};
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

    /// LB protocol generation. Defaults to v2.1 for snapshots persisted
    /// before v2.0 support existed.
    ///
    /// Kept at the end of the struct deliberately: downstream persists this
    /// type with bincode, which is positional and ignores serde defaults.
    /// A trailing field makes an old record fail cleanly at EOF instead of
    /// misparsing every field after it.
    #[serde(default)]
    pub version: LBVersion,
    /// v2.2 only. `Some(non-zero)` means the pair has hooks installed and its
    /// observable behaviour may deviate from pure LB math.
    #[serde(default)]
    pub hooks_parameters: Option<B256>,
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
            version: LBVersion::default(),
            hooks_parameters: None,
        }
    }

    /// Set the protocol version. Chainable after `new`.
    pub fn with_version(mut self, version: LBVersion) -> Self {
        self.version = version;
        self
    }

    /// Set the v2.2 hooks parameters. Chainable after `new`.
    pub fn with_hooks_parameters(mut self, hooks: Option<B256>) -> Self {
        self.hooks_parameters = hooks;
        self
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
                vol_ref = ((self.volatility_accumulator as u64 * self.reduction_factor as u64)
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

    /// Chain time for an event, falling back to wall clock only when the log
    /// carries no block timestamp (some RPCs omit it on pending logs).
    fn log_timestamp(event: &Log) -> u64 {
        event
            .block_timestamp
            .unwrap_or_else(|| chrono::Utc::now().timestamp() as u64)
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

    fn calculate_output_at(
        &self,
        token_in: &Address,
        amount_in: U256,
        ctx: &QuoteContext,
    ) -> Result<U256> {
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
            self.simulate_swap_out_at(amount_in_128, swap_for_y, ctx.timestamp)?;
        if amount_in_left > 0 {
            return Err(anyhow!(
                "Insufficient liquidity in LB pool: {} of {} input remaining",
                amount_in_left,
                amount_in_128
            ));
        }
        Ok(U256::from(amount_out))
    }

    fn calculate_input_at(
        &self,
        token_out: &Address,
        amount_out: U256,
        ctx: &QuoteContext,
    ) -> Result<U256> {
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
            self.simulate_swap_in_at(amount_out_128, swap_for_y, ctx.timestamp)?;
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

                // Mirror LBPair.swap(): updateReferences() runs once, before
                // the bin loop, using PRE-swap state. It reads self.active_id
                // for id_ref and self.time_of_last_update for dt, so it must
                // run before either is overwritten below.
                //
                // Invariant this relies on: one on-chain swap() that crosses N
                // bins emits N Swap logs, so replay calls update_references()
                // N times where the contract called it once. Logs 2..N are
                // no-ops *only because* filter_period > 0 — the first log sets
                // time_of_last_update to the block timestamp, so the rest see
                // dt == 0 and fail the `dt >= self.filter_period` guard in
                // update_references(). With filter_period == 0 that guard
                // would fire on every log and corrupt both references. Every
                // Trader Joe preset in use has filter_period in [10, 300]
                // (30 for this pool), so it is unreachable today, but this
                // call site is only correct while that holds.
                let ts = Self::log_timestamp(event);
                let (vol_ref, id_ref) = self.update_references(ts);
                self.volatility_reference = vol_ref;
                self.id_reference = id_ref;

                // Now apply the event's own state.
                self.active_id = id;
                self.volatility_accumulator = swap_data.volatilityAccumulator.to();
                self.time_of_last_update = ts;

                // last_updated is local bookkeeping — "when this process last
                // touched the pool" — and stays wall clock, same as every
                // other pool type and as apply_swap. Do not collapse this
                // back into time_of_last_update, which mirrors chain state.
                self.last_updated = chrono::Utc::now().timestamp() as u64;
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
            Some(&ILBPairV20::Swap::SIGNATURE_HASH) => {
                let d: ILBPairV20::Swap = event.log_decode()?.inner.data;
                let id: u32 = d.id.to();
                // Fail loudly rather than saturating: an amount that does not
                // fit u128 is malformed, and clamping to u128::MAX would
                // corrupt the bin silently.
                let amount_in: u128 = d
                    .amountIn
                    .try_into()
                    .map_err(|_| anyhow!("v2.0 Swap amountIn exceeds u128"))?;
                let amount_out: u128 = d
                    .amountOut
                    .try_into()
                    .map_err(|_| anyhow!("v2.0 Swap amountOut exceeds u128"))?;

                let (rx, ry) = self.bins.get(&id).copied().unwrap_or((0, 0));
                // swapForY: X goes in, Y comes out.
                let (new_rx, new_ry) = if d.swapForY {
                    (rx.saturating_add(amount_in), ry.saturating_sub(amount_out))
                } else {
                    (rx.saturating_sub(amount_out), ry.saturating_add(amount_in))
                };
                self.update_bin(id, new_rx, new_ry);

                // Same updateReferences ordering as the v2.1+ arm — see the
                // comment there. This MUST run on PRE-swap state, so before
                // active_id / time_of_last_update are overwritten below.
                // Writing `id_reference = id` here instead was the exact bug
                // found on v2.1: it sets the reference to the swap's FINAL bin
                // rather than the pre-swap activeId, and never writes
                // volatility_reference at all.
                let ts = Self::log_timestamp(event);
                let (vol_ref, id_ref) = self.update_references(ts);
                self.volatility_reference = vol_ref;
                self.id_reference = id_ref;

                self.active_id = id;
                self.volatility_accumulator = d.volatilityAccumulated.to();
                self.time_of_last_update = ts;
                // last_updated is local bookkeeping and stays wall clock.
                self.last_updated = chrono::Utc::now().timestamp() as u64;
                Ok(())
            }
            Some(&ILBPairV20::DepositedToBin::SIGNATURE_HASH) => {
                let d: ILBPairV20::DepositedToBin = event.log_decode()?.inner.data;
                let id: u32 = d.id.to();
                let (rx, ry) = self.bins.get(&id).copied().unwrap_or((0, 0));
                self.update_bin(
                    id,
                    rx.saturating_add(d.amountX.try_into().unwrap_or(0)),
                    ry.saturating_add(d.amountY.try_into().unwrap_or(0)),
                );
                self.last_updated = Self::log_timestamp(event);
                Ok(())
            }
            Some(&ILBPairV20::WithdrawnFromBin::SIGNATURE_HASH) => {
                let d: ILBPairV20::WithdrawnFromBin = event.log_decode()?.inner.data;
                let id: u32 = d.id.to();
                let (rx, ry) = self.bins.get(&id).copied().unwrap_or((0, 0));
                self.update_bin(
                    id,
                    rx.saturating_sub(d.amountX.try_into().unwrap_or(0)),
                    ry.saturating_sub(d.amountY.try_into().unwrap_or(0)),
                );
                self.last_updated = Self::log_timestamp(event);
                Ok(())
            }
            Some(&ILBPairV20::CompositionFee::SIGNATURE_HASH) => {
                // UNSETTLED, and deliberately left so. Adding the fee assumes
                // DepositedToBin reports amounts NET of the composition fee,
                // making this the separate accounting entry that puts the fee
                // into the bin. That is wrong if v2.0 instead routes the
                // composition fee down its claimable-fee path — v2.0 has
                // collectFees/pendingFees, where v2.1+ auto-compounds — in
                // which case the deposit already covered it and this
                // double-counts.
                //
                // Nothing decides it today: CompositionFee fires zero times in
                // 500,000 blocks on the v2.0 fixture pool, so no convergence
                // range exercises this arm. The v2.1+ side of the question is
                // settled the other way (its CompositionFees rides the same tx
                // as a fee-INCLUSIVE DepositedToBins, so it gets no handler at
                // all); v2.0 may or may not match. If a widened v2.0 range ever
                // fails on bin reserves, look here first.
                let d: ILBPairV20::CompositionFee = event.log_decode()?.inner.data;
                let id: u32 = d.id.to();
                let (rx, ry) = self.bins.get(&id).copied().unwrap_or((0, 0));
                self.update_bin(
                    id,
                    rx.saturating_add(d.feesX.try_into().unwrap_or(0)),
                    ry.saturating_add(d.feesY.try_into().unwrap_or(0)),
                );
                self.last_updated = Self::log_timestamp(event);
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
            ILBPairV20::Swap::SIGNATURE_HASH,
            ILBPairV20::DepositedToBin::SIGNATURE_HASH,
            ILBPairV20::WithdrawnFromBin::SIGNATURE_HASH,
            ILBPairV20::CompositionFee::SIGNATURE_HASH,
        ]
    }

    fn profitable_topics() -> Vec<Topic> {
        vec![
            ILBPair::Swap::SIGNATURE_HASH,
            ILBPairV20::Swap::SIGNATURE_HASH,
        ]
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::ILBPair;
    use alloy::primitives::{Address, B256};
    use alloy::rpc::types::Log;
    use alloy::sol_types::SolEvent;

    fn pool_with_time(t: u64) -> LBPool {
        let mut p = LBPool::new(
            Address::ZERO,
            Address::ZERO,
            Address::ZERO,
            20,
            8_388_608,
            BTreeMap::new(),
            5_000,
            30,
            600,
            5_000,
            40_000,
            1_000,
            350_000,
            0,
            0,
            8_388_608,
            t,
        );
        p.update_bin(8_388_608, 1_000_000, 1_000_000);
        p
    }

    /// Build a v2.1 Swap log with a known block timestamp.
    fn swap_log(block_timestamp: Option<u64>) -> Log {
        let event = ILBPair::Swap {
            sender: Address::ZERO,
            to: Address::ZERO,
            id: 8_388_608u32.try_into().unwrap(),
            amountsIn: B256::ZERO,
            amountsOut: B256::ZERO,
            volatilityAccumulator: 0u32.try_into().unwrap(),
            totalFees: B256::ZERO,
            protocolFees: B256::ZERO,
        };
        Log {
            inner: alloy::primitives::Log {
                address: Address::ZERO,
                data: event.encode_log_data(),
            },
            block_timestamp,
            ..Default::default()
        }
    }

    #[test]
    fn apply_log_uses_block_timestamp_not_wall_clock() {
        let mut pool = pool_with_time(1_700_000_000);
        pool.apply_log(&swap_log(Some(1_700_000_500))).unwrap();
        assert_eq!(
            pool.time_of_last_update, 1_700_000_500,
            "apply_log must take chain time from the log, not the wall clock"
        );
    }

    #[test]
    fn apply_log_falls_back_to_wall_clock_when_log_has_no_timestamp() {
        let mut pool = pool_with_time(1_700_000_000);
        pool.apply_log(&swap_log(None)).unwrap();
        let now = chrono::Utc::now().timestamp() as u64;
        assert!(
            pool.time_of_last_update.abs_diff(now) < 60,
            "expected a wall-clock fallback near now, got {}",
            pool.time_of_last_update
        );
    }

    /// StaticFeeParametersSet never fires on any live fixture pool — 120,000
    /// blocks scanned, zero occurrences — so no convergence range can cover
    /// this arm. Unit-tested instead, or it would ship unexercised.
    #[test]
    fn apply_log_updates_static_fee_parameters() {
        let mut pool = pool_with_time(1_700_000_000);
        let event = ILBPair::StaticFeeParametersSet {
            sender: Address::ZERO,
            baseFactor: 7777,
            filterPeriod: 44,
            decayPeriod: 888,
            reductionFactor: 4444,
            variableFeeControl: 55555u32.try_into().unwrap(),
            protocolShare: 1234,
            maxVolatilityAccumulator: 222222u32.try_into().unwrap(),
        };
        let log = Log {
            inner: alloy::primitives::Log {
                address: Address::ZERO,
                data: event.encode_log_data(),
            },
            block_timestamp: Some(1_700_000_500),
            ..Default::default()
        };
        pool.apply_log(&log).unwrap();

        assert_eq!(pool.base_factor, 7777);
        assert_eq!(pool.filter_period, 44);
        assert_eq!(pool.decay_period, 888);
        assert_eq!(pool.reduction_factor, 4444);
        assert_eq!(pool.variable_fee_control, 55555);
        assert_eq!(pool.protocol_share, 1234);
        assert_eq!(pool.max_volatility_accumulator, 222222);
    }

    use crate::pool::base::QuoteContext;

    /// A later timestamp decays the volatility accumulator, so quoting the
    /// same input at two different times must be able to differ.
    #[test]
    fn calculate_output_at_honours_the_supplied_timestamp() {
        let mut pool = pool_with_time(1_700_000_000);
        pool.volatility_accumulator = 300_000;
        pool.volatility_reference = 300_000;

        let early = pool
            .calculate_output_at(
                &pool.token_x.clone(),
                U256::from(1_000u64),
                &QuoteContext {
                    timestamp: 1_700_000_000,
                },
            )
            .unwrap();
        let late = pool
            .calculate_output_at(
                &pool.token_x.clone(),
                U256::from(1_000u64),
                &QuoteContext {
                    timestamp: 1_700_100_000,
                },
            )
            .unwrap();

        assert!(early <= late, "decayed volatility should not raise the fee");
    }

    #[test]
    fn topics_cover_both_generations_without_collision() {
        let topics = LBPool::topics();
        assert!(topics.contains(&ILBPair::Swap::SIGNATURE_HASH));
        assert!(topics.contains(&crate::contracts::ILBPairV20::Swap::SIGNATURE_HASH));

        let mut sorted = topics.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), topics.len(), "duplicate topic registered");
    }

    /// Build a v2.0 Swap log.
    fn v20_swap_log(
        id: u32,
        swap_for_y: bool,
        amount_in: u128,
        amount_out: u128,
        vol_acc: u32,
        block_timestamp: u64,
    ) -> Log {
        let event = crate::contracts::ILBPairV20::Swap {
            sender: Address::ZERO,
            recipient: Address::ZERO,
            id: U256::from(id),
            swapForY: swap_for_y,
            amountIn: U256::from(amount_in),
            amountOut: U256::from(amount_out),
            volatilityAccumulated: U256::from(vol_acc),
            fees: U256::ZERO,
        };
        Log {
            inner: alloy::primitives::Log {
                address: Address::ZERO,
                data: event.encode_log_data(),
            },
            block_timestamp: Some(block_timestamp),
            ..Default::default()
        }
    }

    /// The v2.0 Swap arm must call update_references() on PRE-swap state:
    /// id_reference takes the pre-swap active_id (not the swap's final bin),
    /// and volatility_reference is written from the decayed accumulator.
    #[test]
    fn v20_swap_updates_references_before_overwriting_state() {
        let mut pool = pool_with_time(1_700_000_000);
        pool.volatility_accumulator = 100_000;
        pool.update_bin(8_388_610, 0, 2_000);

        // dt = 100: past filter_period (30), inside decay_period (600).
        pool.apply_log(&v20_swap_log(
            8_388_610,
            true,
            1_000,
            1_500,
            77_777,
            1_700_000_100,
        ))
        .unwrap();

        // References were computed from pre-swap state.
        assert_eq!(
            pool.id_reference, 8_388_608,
            "id_reference must be the PRE-swap active_id, not the event's id"
        );
        assert_eq!(
            pool.volatility_reference, 50_000,
            "volatility_reference must decay the PRE-swap accumulator"
        );

        // Then the event's own state was applied.
        assert_eq!(pool.active_id, 8_388_610);
        assert_eq!(pool.volatility_accumulator, 77_777);
        assert_eq!(pool.time_of_last_update, 1_700_000_100);

        // swapForY: X in, Y out.
        assert_eq!(pool.bins.get(&8_388_610).copied(), Some((1_000, 500)));
    }

    /// v2.0 deposit/withdraw/composition-fee arms move the right bin.
    #[test]
    fn v20_liquidity_events_update_bins() {
        use crate::contracts::ILBPairV20;

        fn log_of<E: SolEvent>(event: E) -> Log {
            Log {
                inner: alloy::primitives::Log {
                    address: Address::ZERO,
                    data: event.encode_log_data(),
                },
                block_timestamp: Some(1_700_000_100),
                ..Default::default()
            }
        }

        let mut pool = pool_with_time(1_700_000_000);

        pool.apply_log(&log_of(ILBPairV20::DepositedToBin {
            sender: Address::ZERO,
            recipient: Address::ZERO,
            id: U256::from(8_388_608u32),
            amountX: U256::from(500u64),
            amountY: U256::from(700u64),
        }))
        .unwrap();
        assert_eq!(
            pool.bins.get(&8_388_608).copied(),
            Some((1_000_500, 1_000_700))
        );

        pool.apply_log(&log_of(ILBPairV20::WithdrawnFromBin {
            sender: Address::ZERO,
            recipient: Address::ZERO,
            id: U256::from(8_388_608u32),
            amountX: U256::from(500u64),
            amountY: U256::from(700u64),
        }))
        .unwrap();
        assert_eq!(
            pool.bins.get(&8_388_608).copied(),
            Some((1_000_000, 1_000_000))
        );

        pool.apply_log(&log_of(ILBPairV20::CompositionFee {
            sender: Address::ZERO,
            recipient: Address::ZERO,
            id: U256::from(8_388_608u32),
            feesX: U256::from(3u64),
            feesY: U256::from(4u64),
        }))
        .unwrap();
        assert_eq!(
            pool.bins.get(&8_388_608).copied(),
            Some((1_000_003, 1_000_004))
        );
    }
}
