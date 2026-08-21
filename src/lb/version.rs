//! LB protocol version and runtime detection.

use serde::{Deserialize, Serialize};

/// Trader Joe Liquidity Book protocol generation.
///
/// Swap, fee, and price math is identical across all three. Version affects
/// only state fetching, event decoding, and hooks awareness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LBVersion {
    /// Original LB. Distinct getters and event ABIs from v2.1+.
    V2_0,
    /// Bin tree at storage slot 8 (ReentrancyGuard `_status` shifts slots by 1).
    V2_1,
    /// Bin tree at storage slot 7. Adds hooks. Event ABIs identical to v2.1.
    V2_2,
}

impl Default for LBVersion {
    fn default() -> Self {
        Self::V2_1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_v21() {
        // Snapshots persisted by evm-dex-pool 1.4.0 predate v2.0 support,
        // so an absent version must deserialize as v2.1.
        assert_eq!(LBVersion::default(), LBVersion::V2_1);
    }

    #[test]
    fn round_trips_through_serde() {
        for v in [LBVersion::V2_0, LBVersion::V2_1, LBVersion::V2_2] {
            let json = serde_json::to_string(&v).unwrap();
            let back: LBVersion = serde_json::from_str(&json).unwrap();
            assert_eq!(v, back);
        }
    }

    use crate::lb::LBPool;
    use alloy::primitives::Address;
    use std::collections::BTreeMap;

    #[test]
    fn pool_without_version_field_deserializes_as_v21() {
        // Build a pool, serialize it, then strip `version` to simulate a
        // snapshot written by evm-dex-pool 1.4.0.
        let pool = LBPool::new(
            Address::ZERO, Address::ZERO, Address::ZERO,
            20, 8_388_608, BTreeMap::new(),
            5_000, 30, 600, 5_000, 40_000, 1_000, 350_000,
            0, 0, 8_388_608, 0,
        );
        let mut value: serde_json::Value = serde_json::to_value(&pool).unwrap();
        value.as_object_mut().unwrap().remove("version");
        value.as_object_mut().unwrap().remove("hooks_parameters");

        let restored: LBPool = serde_json::from_value(value).unwrap();
        assert_eq!(restored.version, LBVersion::V2_1);
        assert_eq!(restored.hooks_parameters, None);
    }
}
