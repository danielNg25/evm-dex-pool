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

/// Where a swap step may end, besides the target price.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickSearch {
    /// Uniswap V3's `TickBitmap.nextInitializedTickWithinOneWord`: the next
    /// initialized tick, but never past the end of the current 256-tick word
    /// (ticks counted in units of `tick_spacing`). Uniswap V3 and its forks
    /// (PancakeV3, Ramses CL) end a step at every word edge, and each step
    /// rounds its input and fee up, so skipping the edge overstated the output
    /// by one to two input units -- 49 of run 8's 99 non-exact replays, up to
    /// 3.6% of a cycle's profit where one unit is a satoshi.
    WordBounded { tick_spacing: i32 },
    /// The next initialized tick wherever it is. Algebra Integral walks a
    /// linked list of initialized ticks and never stops at a word edge.
    NextInitialized,
}

pub trait TickDataProvider {
    /// Return information corresponding to a specific tick
    ///
    /// ## Arguments
    ///
    /// * `tick`: The tick to load
    ///
    /// returns: Result<&Tick>
    fn get_tick(&self, tick: i32) -> Result<&Tick>;

    /// Where the next swap step ends: `(tick, initialized)`. `lte` searches
    /// downward (zeroForOne) and includes `tick` itself, as on chain; `search`
    /// says whether a bitmap word edge ends the step too.
    fn next_initialized_tick(
        &self,
        tick: i32,
        lte: bool,
        search: TickSearch,
    ) -> Result<(i32, bool)>;
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

    fn next_initialized_tick(
        &self,
        tick: i32,
        lte: bool,
        search: TickSearch,
    ) -> Result<(i32, bool)> {
        match search {
            TickSearch::NextInitialized => {
                if lte {
                    Ok(self
                        .range(..=tick)
                        .next_back()
                        .map_or((tick, false), |(&t, _)| (t, true)))
                } else {
                    Ok(self
                        .range(tick + 1..)
                        .next()
                        .map_or((tick, false), |(&t, _)| (t, true)))
                }
            }
            TickSearch::WordBounded { tick_spacing } => {
                if tick_spacing <= 0 {
                    return Err(anyhow::anyhow!(
                        "tick_spacing must be positive, got {tick_spacing}"
                    ));
                }
                // Compress to units of tick_spacing, rounding toward negative
                // infinity like TickBitmap; a word is 256 compressed ticks.
                let compressed = tick.div_euclid(tick_spacing);
                if lte {
                    let word_start = (compressed >> 8) << 8;
                    let (lo, hi) = (word_start * tick_spacing, compressed * tick_spacing);
                    Ok(self
                        .range(lo..=hi)
                        .next_back()
                        .map_or((lo, false), |(&t, _)| (t, true)))
                } else {
                    let next = compressed + 1;
                    let word_end = ((next >> 8) << 8) + 255;
                    let (lo, hi) = (next * tick_spacing, word_end * tick_spacing);
                    Ok(self
                        .range(lo..=hi)
                        .next()
                        .map_or((hi, false), |(&t, _)| (t, true)))
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn map(ticks: &[i32]) -> TickMap {
        ticks
            .iter()
            .map(|&index| {
                (
                    index,
                    Tick {
                        index,
                        liquidity_net: 1,
                        liquidity_gross: 1,
                    },
                )
            })
            .collect()
    }

    // With tick_spacing 60 a word holds compressed ticks 256*w ..= 256*w + 255:
    // word 0 is ticks 0 ..= 15300, word 1 is 15360 ..= 30660, word -1 is
    // -15360 ..= -60.

    #[test]
    fn word_bounded_upward_stops_at_the_word_end() {
        let m = map(&[-600, 600, 30_000]);
        let w = TickSearch::WordBounded { tick_spacing: 60 };
        assert_eq!(m.next_initialized_tick(0, false, w).unwrap(), (600, true));
        assert_eq!(
            m.next_initialized_tick(600, false, w).unwrap(),
            (15_300, false)
        );
        assert_eq!(
            m.next_initialized_tick(15_300, false, w).unwrap(),
            (30_000, true)
        );
    }

    #[test]
    fn word_bounded_downward_includes_the_current_tick_and_stops_at_the_word_start() {
        let m = map(&[-600, 600]);
        let w = TickSearch::WordBounded { tick_spacing: 60 };
        assert_eq!(m.next_initialized_tick(600, true, w).unwrap(), (600, true));
        assert_eq!(m.next_initialized_tick(599, true, w).unwrap(), (0, false));
        assert_eq!(m.next_initialized_tick(-1, true, w).unwrap(), (-600, true));
        assert_eq!(
            m.next_initialized_tick(-601, true, w).unwrap(),
            (-15_360, false)
        );
    }

    #[test]
    fn next_initialized_ignores_word_edges() {
        let m = map(&[-600, 30_000]);
        let s = TickSearch::NextInitialized;
        assert_eq!(
            m.next_initialized_tick(0, false, s).unwrap(),
            (30_000, true)
        );
        assert_eq!(m.next_initialized_tick(0, true, s).unwrap(), (-600, true));
    }
}
