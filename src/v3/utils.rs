use alloy::primitives::{uint, U160, U256};

pub(crate) const ONE: U160 = uint!(1_U160);
pub(crate) const TWO: U256 = uint!(2_U256);
pub(crate) const THREE: U256 = uint!(3_U256);
pub const Q96: U256 = U256::from_limbs([0, 1 << 32, 0, 0]);
pub const Q128: U256 = U256::from_limbs([0, 0, 1, 0]);
pub const Q192: U256 = U256::from_limbs([0, 0, 0, 1]);

/// Convert a tick to its corresponding word index in the tick bitmap
pub fn tick_to_word(tick: i32, tick_spacing: i32) -> i32 {
    let compressed = tick / tick_spacing;
    let compressed = if tick < 0 && tick % tick_spacing != 0 {
        compressed - 1
    } else {
        compressed
    };
    compressed >> 8
}

/// Convert a word index back to the tick range it represents
pub fn word_to_tick_range(word: i32, tick_spacing: i32) -> (i32, i32) {
    let min_tick = (word << 8) * tick_spacing;
    let max_tick = ((word + 1) << 8) * tick_spacing - 1;
    (min_tick, max_tick)
}
