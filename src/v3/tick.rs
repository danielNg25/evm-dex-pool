use alloy::primitives::aliases::I24;
use anyhow::Result;
use core::{
    fmt::Debug,
    hash::Hash,
    ops::{Add, BitAnd, Div, Mul, Rem, Shl, Shr, Sub},
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
/// Struct containing information about a tick in a V3 pool
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tick {
    /// The tick index
    pub index: i32,
    /// The liquidity net value (positive for mint, negative for burn)
    pub liquidity_net: i128,
    /// The liquidity gross value
    pub liquidity_gross: u128,
}

pub type TickMap = BTreeMap<i32, Tick>;

pub trait TickDataProvider {
    /// Return information corresponding to a specific tick
    ///
    /// ## Arguments
    ///
    /// * `tick`: The tick to load
    ///
    /// returns: Result<&Tick>
    fn get_tick(&self, tick: i32) -> Result<&Tick>;

    /// Return the next tick that is initialized within a single word
    ///
    /// ## Arguments
    ///
    /// * `tick`: The current tick
    /// * `lte`: Whether the next tick should be lte the current tick
    ///
    /// returns: Result<(i32, bool)>
    fn next_initialized_tick_within_one_word(&self, tick: i32, lte: bool) -> Result<(i32, bool)>;
}

/// Provides information about ticks
impl TickDataProvider for TickMap {
    /// Return information corresponding to a specific tick
    ///
    /// ## Arguments
    ///
    /// * `tick`: The tick to load
    ///
    /// returns: Result<&Tick<i32>>
    fn get_tick(&self, tick: i32) -> Result<&Tick> {
        self.get(&tick)
            .ok_or_else(|| anyhow::anyhow!("Tick not found"))
    }

    /// Return the next tick that is initialized within a single word
    ///
    /// ## Arguments
    ///
    /// * `tick`: The current tick
    /// * `lte`: Whether the next tick should be lte the current tick
    /// * `tick_spacing`: The tick spacing of the pool
    ///
    /// returns: Result<(i32, bool)>
    fn next_initialized_tick_within_one_word(&self, tick: i32, lte: bool) -> Result<(i32, bool)> {
        if lte {
            // Find the greatest tick less than or equal to the current tick
            if let Some((&next_tick, _)) = self.range(..=tick).next_back() {
                Ok((next_tick, true))
            } else {
                Ok((tick, false))
            }
        } else {
            // Find the smallest tick greater than the current tick
            if let Some((&next_tick, _)) = self.range(tick + 1..).next() {
                Ok((next_tick, true))
            } else {
                Ok((tick, false))
            }
        }
    }
}

/// The trait for tick indexes used across [`Tick`], [`TickDataProvider`], and [`TickList`].
///
/// Implemented for [`i32`] and [`Signed`].
pub trait TickIndex:
    Copy
    + Debug
    + Default
    + Hash
    + Ord
    + BitAnd<Output = Self>
    + Add<Output = Self>
    + Div<Output = Self>
    + Mul<Output = Self>
    + Rem<Output = Self>
    + Sub<Output = Self>
    + Shl<i32, Output = Self>
    + Shr<i32, Output = Self>
    + TryFrom<i32, Error: Debug>
    + TryInto<i32, Error: Debug>
{
    const ZERO: Self;
    const ONE: Self;

    #[inline]
    fn is_zero(self) -> bool {
        self == Self::ZERO
    }

    fn from_i24(value: I24) -> Self;

    fn to_i24(self) -> I24;

    #[inline]
    fn compress(self, tick_spacing: Self) -> Self {
        assert!(tick_spacing > Self::ZERO, "TICK_SPACING");
        if self % tick_spacing < Self::ZERO {
            self / tick_spacing - Self::ONE
        } else {
            self / tick_spacing
        }
    }

    #[inline]
    fn position(self) -> (Self, u8) {
        (
            self >> 8,
            (self & Self::try_from(0xff).unwrap()).try_into().unwrap() as u8,
        )
    }
}

impl TickIndex for i32 {
    const ZERO: Self = 0;
    const ONE: Self = 1;

    #[inline]
    fn from_i24(value: I24) -> Self {
        value.as_i32()
    }

    #[inline]
    fn to_i24(self) -> I24 {
        I24::try_from(self).unwrap()
    }
}
