//! Per-chain Trader Joe Liquidity Book factory addresses.
//!
//! Mirrors the shape of [`crate::v2::factories`]: a nested `factories` module
//! holding one submodule per chain, plus lookup helpers keyed by chain ID.
//! Adding a chain is a data edit — a new submodule and one arm in each
//! `match` below.
//!
//! # What this is for
//!
//! A consumer that discovers pools by watching factory logs needs two things:
//! the creation topic (registered in [`crate::pool::base::POOL_CREATED_TOPICS`]
//! as `ILBFactory::LBPairCreated`) and the addresses to watch. This module is
//! the second half. It deliberately stops there — decoding a creation log and
//! feeding the new pair into `add_pools` is the consumer's job, not this
//! crate's.
//!
//! # Verification status
//!
//! Every entry carries [`LBFactory::verified`]. It is `true` only for
//! addresses confirmed against a live chain by this crate's authors, and the
//! doc comment on each entry says how. Treat `verified: false` as
//! "plausible, from published documentation, never checked" — those addresses
//! come from Trader Joe's claim that the v2.1 and v2.2 factories were deployed
//! deterministically to the same address on every chain except Ethereum
//! mainnet v2.1. That claim has not been checked here on any chain other than
//! Avalanche, and v2.0's factories are per-chain with no such pattern at all.

use crate::lb::version::LBVersion;
use alloy::primitives::Address;

/// One Trader Joe LB factory deployment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LBFactory {
    /// Factory contract address, as a hex string. Case-insensitive — compare
    /// with [`LBFactory::matches`] or parse with [`LBFactory::parse`].
    pub address: &'static str,
    /// Which LB generation this factory deploys.
    pub version: LBVersion,
    /// `true` only when this exact address was confirmed against the live
    /// chain. See the module docs.
    pub verified: bool,
}

impl LBFactory {
    /// Parse the address. Returns `None` for a malformed literal, which would
    /// be a typo in the table below.
    pub fn parse(&self) -> Option<Address> {
        self.address.parse().ok()
    }

    /// Case-insensitive address comparison against a hex string.
    pub fn matches(&self, address_str: &str) -> bool {
        self.address.eq_ignore_ascii_case(address_str)
    }
}

/// Chain-specific LB factory addresses.
pub mod factories {
    use super::LBFactory;
    use crate::lb::version::LBVersion;

    pub mod avalanche {
        use super::{LBFactory, LBVersion};
        pub const CHAIN_ID: u64 = 43114;

        /// All three verified live on Avalanche C-Chain, two ways:
        ///
        /// 1. Each address holds deployed bytecode (24,044 / 21,176 / 21,674
        ///    bytes for v2.2 / v2.1 / v2.0).
        /// 2. Each is what a known pair of that generation names as its own
        ///    factory: `getFactory()` on the v2.2 pair
        ///    `0x8573f98175d816d520248b5facf40d309b1c9cee` returns the v2.2
        ///    address, `getFactory()` on the v2.1 pair
        ///    `0x4224f6f4c9280509724db2dbac314621e4465c29` returns the v2.1
        ///    address, and `factory()` on the v2.0 pair
        ///    `0x18332988456C4Bd9ABa6698ec748b331516F5A14` returns the v2.0
        ///    address. Those three pairs are this crate's convergence-test
        ///    fixtures, so the mapping is version-correct and not just
        ///    "something is deployed here".
        pub const LB_FACTORIES: &[LBFactory] = &[
            LBFactory {
                address: "0xb43120c4745967fa9b93E79C149E66B0f2D6Fe0c",
                version: LBVersion::V2_2,
                verified: true,
            },
            LBFactory {
                address: "0x8e42f2F4101563bF679975178e880FD87d3eFd4e",
                version: LBVersion::V2_1,
                verified: true,
            },
            LBFactory {
                address: "0x6E77932A92582f504FF6c4BdbCef7Da6c198aEEf",
                version: LBVersion::V2_0,
                verified: true,
            },
        ];
    }

    pub mod arbitrum {
        use super::{LBFactory, LBVersion};
        pub const CHAIN_ID: u64 = 42161;

        /// UNVERIFIED. Assumes the deterministic v2.1/v2.2 addresses. No v2.0
        /// entry: v2.0 factories are per-chain and this crate has no
        /// corroborated Arbitrum address for one. Add it as a data edit once
        /// somebody checks `factory()` on a live Arbitrum v2.0 pair.
        pub const LB_FACTORIES: &[LBFactory] = &[
            LBFactory {
                address: "0xb43120c4745967fa9b93E79C149E66B0f2D6Fe0c",
                version: LBVersion::V2_2,
                verified: false,
            },
            LBFactory {
                address: "0x8e42f2F4101563bF679975178e880FD87d3eFd4e",
                version: LBVersion::V2_1,
                verified: false,
            },
        ];
    }

    pub mod bsc {
        use super::{LBFactory, LBVersion};
        pub const CHAIN_ID: u64 = 56;

        /// UNVERIFIED. Same deterministic-address assumption as Arbitrum.
        pub const LB_FACTORIES: &[LBFactory] = &[
            LBFactory {
                address: "0xb43120c4745967fa9b93E79C149E66B0f2D6Fe0c",
                version: LBVersion::V2_2,
                verified: false,
            },
            LBFactory {
                address: "0x8e42f2F4101563bF679975178e880FD87d3eFd4e",
                version: LBVersion::V2_1,
                verified: false,
            },
        ];
    }

    pub mod ethereum {
        use super::{LBFactory, LBVersion};
        pub const CHAIN_ID: u64 = 1;

        /// UNVERIFIED, and the one documented exception to the deterministic
        /// address: mainnet's v2.1 factory is reported at a different address
        /// from every other chain's. If exactly one entry in this table is
        /// wrong, expect it to be this one — it is the only place the
        /// "same address everywhere" shortcut does not apply, so it is the
        /// only place a copy-paste from another chain would be silently wrong.
        pub const LB_FACTORIES: &[LBFactory] = &[
            LBFactory {
                address: "0xb43120c4745967fa9b93E79C149E66B0f2D6Fe0c",
                version: LBVersion::V2_2,
                verified: false,
            },
            LBFactory {
                address: "0xDC8d77b69155c7E68A95a4fb0f06a71FF90B943a",
                version: LBVersion::V2_1,
                verified: false,
            },
        ];
    }

    /// Empty slice returned for chains with no known LB deployment.
    pub const NONE: &[LBFactory] = &[];
}

/// All known LB factories for a chain. Empty if the chain has no LB
/// deployment recorded here — which is not proof there is none.
pub fn get_lb_factories_by_chain_id(chain_id: u64) -> &'static [LBFactory] {
    match chain_id {
        1 => factories::ethereum::LB_FACTORIES,
        56 => factories::bsc::LB_FACTORIES,
        42161 => factories::arbitrum::LB_FACTORIES,
        43114 => factories::avalanche::LB_FACTORIES,
        _ => factories::NONE,
    }
}

/// Parsed factory addresses for a chain, for use as an `eth_getLogs` address
/// filter alongside `ILBFactory::LBPairCreated`.
pub fn get_lb_factory_addresses_by_chain_id(chain_id: u64) -> Vec<Address> {
    get_lb_factories_by_chain_id(chain_id)
        .iter()
        .filter_map(|f| f.parse())
        .collect()
}

/// Which LB generation a factory address deploys, or `None` if the address is
/// not a known LB factory on that chain.
///
/// A `LBPairCreated` log names the pair but not its generation, and the three
/// generations need different fetchers. This resolves it from the emitter
/// without an RPC round trip; `crate::lb::detect_lb_version` remains the
/// authority when the emitter is unknown.
pub fn get_lb_version_by_factory(chain_id: u64, address: &str) -> Option<LBVersion> {
    get_lb_factories_by_chain_id(chain_id)
        .iter()
        .find(|f| f.matches(address))
        .map(|f| f.version)
}

/// Chain IDs with LB factory data in this table.
pub fn get_lb_supported_chain_ids() -> Vec<u64> {
    vec![1, 56, 42161, 43114]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every literal in the table must parse. A typo'd address would
    /// otherwise be dropped silently by `filter_map` in
    /// `get_lb_factory_addresses_by_chain_id`, producing a discovery filter
    /// that quietly watches fewer factories than it claims to.
    #[test]
    fn every_factory_address_parses() {
        for chain_id in get_lb_supported_chain_ids() {
            let entries = get_lb_factories_by_chain_id(chain_id);
            assert!(!entries.is_empty(), "chain {chain_id} listed but empty");
            for f in entries {
                assert!(
                    f.parse().is_some(),
                    "chain {chain_id}: unparseable factory address {}",
                    f.address
                );
            }
            assert_eq!(
                entries.len(),
                get_lb_factory_addresses_by_chain_id(chain_id).len(),
                "chain {chain_id}: an address failed to parse and was dropped"
            );
        }
    }

    /// The only entries claiming `verified: true` are the three Avalanche
    /// addresses whose doc comment records how they were checked. This test
    /// exists so that adding an unchecked address with `verified: true`
    /// copy-pasted from a neighbouring entry fails here instead of quietly
    /// laundering a guess into a fact.
    #[test]
    fn only_avalanche_entries_claim_verification() {
        for chain_id in get_lb_supported_chain_ids() {
            for f in get_lb_factories_by_chain_id(chain_id) {
                if f.verified {
                    assert_eq!(
                        chain_id, 43114,
                        "chain {chain_id}: {} claims verified but only the \
                         Avalanche addresses have been checked live",
                        f.address
                    );
                }
            }
        }
    }

    #[test]
    fn resolves_version_from_avalanche_factory_case_insensitively() {
        assert_eq!(
            get_lb_version_by_factory(43114, "0xb43120c4745967fa9b93e79c149e66b0f2d6fe0c"),
            Some(LBVersion::V2_2)
        );
        assert_eq!(
            get_lb_version_by_factory(43114, "0x8E42F2F4101563BF679975178E880FD87D3EFD4E"),
            Some(LBVersion::V2_1)
        );
        assert_eq!(
            get_lb_version_by_factory(43114, "0x6E77932A92582f504FF6c4BdbCef7Da6c198aEEf"),
            Some(LBVersion::V2_0)
        );
        assert_eq!(
            get_lb_version_by_factory(43114, "0x0000000000000000000000000000000000000001"),
            None
        );
    }

    /// Mainnet's v2.1 factory is the documented exception to the
    /// deterministic address. Guard it so a future "tidy-up" that unifies the
    /// table on one address per version is caught.
    #[test]
    fn ethereum_v21_differs_from_every_other_chain() {
        let eth_v21 = get_lb_factories_by_chain_id(1)
            .iter()
            .find(|f| f.version == LBVersion::V2_1)
            .expect("ethereum v2.1 entry");
        let avax_v21 = get_lb_factories_by_chain_id(43114)
            .iter()
            .find(|f| f.version == LBVersion::V2_1)
            .expect("avalanche v2.1 entry");
        assert!(!eth_v21.matches(avax_v21.address));
    }

    #[test]
    fn unknown_chain_yields_no_factories() {
        assert!(get_lb_factories_by_chain_id(1337).is_empty());
        assert!(get_lb_factory_addresses_by_chain_id(1337).is_empty());
        assert_eq!(
            get_lb_version_by_factory(1337, "0xb43120c4745967fa9b93E79C149E66B0f2D6Fe0c"),
            None
        );
    }
}
