//! RPC-enabled contract definitions for pool fetching.
//! Only compiled when the `rpc` feature is enabled.

use alloy::sol;

// V2 pool contracts (for fetching)
sol! {
    #[sol(rpc)]
    RpcIUniswapV2Pair,
    "contracts/ABI/IUniswapV2Pair.json"
}

sol! {
    #[sol(rpc)]
    RpcIV2PairUint256,
    "contracts/ABI/IV2PairUint256.json"
}

sol! {
    #[sol(rpc)]
    RpcVolatileStableFeeInFactory,
    "contracts/ABI/UniswapV2PairVolatileStableFeeInFactory.json"
}

sol! {
    #[sol(rpc)]
    RpcVolatileStableGetFee,
    "contracts/ABI/UniswapV2PairVolatileStableGetFee.json"
}

sol! {
    #[sol(rpc)]
    RpcUniswapV2FactoryGetFeePool,
    "contracts/ABI/UniswapV2FactoryGetFeePool.json"
}

sol! {
    #[sol(rpc)]
    RpcUniswapV2FactoryPairFee,
    "contracts/ABI/UniswapV2FactoryPairFee.json"
}

sol! {
    #[sol(rpc)]
    RpcUniswapV2FactoryGetFeeOnlyPair,
    "contracts/ABI/UniswapV2FactoryGetFeeOnlyPair.json"
}

sol! {
    #[sol(rpc)]
    RpcIVeloPoolFactory,
    "contracts/ABI/IVeloPoolFactory.json"
}

// V3 pool contracts (for fetching)
sol! {
    #[sol(rpc)]
    RpcIUniswapV3Pool,
    "contracts/ABI/IUniswapV3Pool.json"
}

sol! {
    #[sol(rpc)]
    RpcCLPPool,
    "contracts/ABI/CLPPool.json"
}

sol! {
    #[sol(rpc)]
    RpcAlgebraV3Pool,
    "contracts/ABI/AlgebraV3.json"
}

sol! {
    #[sol(rpc)]
    RpcAlgebraTwoSideFee,
    "contracts/ABI/AlgebraTwoSideFee.json"
}

sol! {
    #[sol(rpc)]
    RpcAlgebraPoolFeeInState,
    "contracts/ABI/AlgebraPoolFeeInState.json"
}

sol! {
    #[sol(rpc)]
    RpcIQuoter,
    "contracts/ABI/IQuoter.json"
}

// LB pool contracts (for fetching)
sol! {
    #[sol(rpc)]
    RpcILBPair,
    "contracts/ABI/ILBPair.json"
}

sol! {
    #[sol(rpc)]
    RpcILBPairV20,
    "contracts/ABI/ILBPairV20.json"
}

// ERC4626 contracts (for fetching)
sol! {
    #[sol(rpc)]
    RpcIERC4626,
    "contracts/ABI/IERC4626.json"
}

sol! {
    #[sol(rpc)]
    RpcIERC20,
    "contracts/ABI/IERC20.json"
}

// Verio IP contracts (for fetching)
sol! {
    #[sol(rpc)]
    RpcIVerioIP,
    "contracts/ABI/IVerioIP.json"
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::hex;
    use alloy::sol_types::{SolCall, SolEvent};

    /// Selectors observed on deployed LB pairs. If a hand-written ABI entry
    /// has the wrong parameter types, its computed selector will not match.
    #[test]
    fn v20_selectors_match_deployed_contracts() {
        assert_eq!(RpcILBPairV20::getReservesAndIdCall::SELECTOR, hex!("1b05b83e"));
        assert_eq!(RpcILBPairV20::feeParametersCall::SELECTOR, hex!("98c7adf3"));
        assert_eq!(RpcILBPairV20::findFirstNonEmptyBinIdCall::SELECTOR, hex!("8f919a83"));
    }

    #[test]
    fn v21_selectors_match_deployed_contracts() {
        assert_eq!(RpcILBPair::getReservesCall::SELECTOR, hex!("0902f1ac"));
        assert_eq!(RpcILBPair::getActiveIdCall::SELECTOR, hex!("dbe65edc"));
        assert_eq!(RpcILBPair::getBinStepCall::SELECTOR, hex!("17f11ecc"));
        assert_eq!(RpcILBPair::getStaticFeeParametersCall::SELECTOR, hex!("7ca0de30"));
        assert_eq!(RpcILBPair::getVariableFeeParametersCall::SELECTOR, hex!("8d7024e5"));
    }

    /// getLBHooksParameters() is the sole discriminator Task 3 uses to detect v2.2:
    /// it answers on v2.2 pairs and reverts on v2.1 pairs. Selector verified via
    /// eth_call against both a live v2.2 pair (returns data) and a live v2.1 pair
    /// (reverts) on Avalanche C-Chain -- see the fix report for the raw responses.
    #[test]
    fn get_lb_hooks_parameters_selector_matches_deployed_contract() {
        assert_eq!(RpcILBPair::getLBHooksParametersCall::SELECTOR, hex!("781a8915"));
    }

    /// Real topic0 values observed on deployed LB pairs via eth_getLogs. Asserting
    /// equality (not just inequality) means a mistyped parameter -- e.g. v2.0's `id`
    /// accidentally narrowed to `uint24` to match v2.1's analogous field -- is caught
    /// here instead of silently producing a topic0 that merely happens to differ.
    #[test]
    fn swap_topics_match_deployed_contracts() {
        assert_eq!(
            crate::contracts::ILBPair::Swap::SIGNATURE_HASH,
            hex!("ad7d6f97abf51ce18e17a38f4d70e975be9c0708474987bb3e26ad21bd93ca70")
        );
        assert_eq!(
            crate::contracts::ILBPairV20::Swap::SIGNATURE_HASH,
            hex!("c528cda9e500228b16ce84fadae290d9a49aecb17483110004c5af0a07f6fd73")
        );
        assert_ne!(
            crate::contracts::ILBPair::Swap::SIGNATURE_HASH,
            crate::contracts::ILBPairV20::Swap::SIGNATURE_HASH
        );
    }

    #[test]
    fn v21_composition_fees_event_is_bound() {
        assert_eq!(
            crate::contracts::ILBPair::CompositionFees::SIGNATURE,
            "CompositionFees(address,uint24,bytes32,bytes32)"
        );
        assert_eq!(
            crate::contracts::ILBPair::CompositionFees::SIGNATURE_HASH,
            hex!("3f0b46725027bb418b2005f4683538eccdbcdf1de2b8649a29dbd9c507d16ff4")
        );
    }

    /// Ground truth, from a live Avalanche log: tx
    /// `0xb4aa0a25f7052b11f2d7dd77932cb4a844c9f8dfd58682d3148e91f8dd863fa2`,
    /// block 93,502,199, pair `0x4224f6f4c9280509724db2dbac314621e4465c29`,
    /// logIndex 13. Two indexed topics and four data words; word 0 is
    /// `0x801a00` = 8,395,264 (the activeId, matching the bin that moved) and
    /// word 2 is `totalFees` packed as `(x = 1, y = 0)`.
    ///
    /// The hash below is what the chain emitted. If `contracts/ABI/ILBPair.json`
    /// ever produces a different one, the ABI is wrong — do not "fix" this
    /// constant.
    ///
    /// v2.0 has no counterpart asserted here because
    /// `contracts/ABI/ILBPairV20.json` declares no flash-loan event and none
    /// was captured off-chain to derive one from — see
    /// `flash_loan_has_no_v20_counterpart` in `src/lb/pool.rs`.
    #[test]
    fn v21_flash_loan_topic_matches_deployed_contract() {
        assert_eq!(
            crate::contracts::ILBPair::FlashLoan::SIGNATURE,
            "FlashLoan(address,address,uint24,bytes32,bytes32,bytes32)"
        );
        assert_eq!(
            crate::contracts::ILBPair::FlashLoan::SIGNATURE_HASH,
            hex!("d126bd9d94daca8e55ffd8283fac05394aec8326c6b1639e1e8a445fbe8bbc7d")
        );
    }

    #[test]
    fn v21_withdrawn_from_bins_topic_matches_deployed_contract() {
        assert_eq!(
            crate::contracts::ILBPair::WithdrawnFromBins::SIGNATURE_HASH,
            hex!("a32e146844d6144a22e94c586715a1317d58a8aa3581ec33d040113ddcb24350")
        );
    }

    /// Ground truth, from a live Avalanche log decoded off the v2.2 factory
    /// `0xb43120c4745967fa9b93E79C149E66B0f2D6Fe0c` in the window ending at
    /// block 91,408,504: three indexed topics (tokenX
    /// `0x30d83929d743a5f28108fb394f8187d54ea804d6`, tokenY WAVAX
    /// `0xb31f66aa3c1e785363f0875a1b74e27b85fd66c7`, binStep `10`) and two
    /// data words (LBPair `0x8d8591de161bb901549f569676f2d2d4687e5210`, pid
    /// `41200`) — matching this crate's `ILBFactory::LBPairCreated` binding
    /// field-for-field.
    ///
    /// The hash below is what the chain emitted. If
    /// `contracts/ABI/ILBFactory.json` ever produces a different one, the
    /// ABI is wrong — do not "fix" this constant. See
    /// `tests/lb_discovery.rs` for a live-fetch integration test that
    /// decodes a fresh log through this same binding.
    #[test]
    fn lb_pair_created_topic_matches_deployed_contract() {
        assert_eq!(
            crate::contracts::ILBFactory::LBPairCreated::SIGNATURE,
            "LBPairCreated(address,address,uint256,address,uint256)"
        );
        assert_eq!(
            crate::contracts::ILBFactory::LBPairCreated::SIGNATURE_HASH,
            hex!("2c8d104b27c6b7f4492017a6f5cf3803043688934ebcaa6a03540beeaf976aff")
        );
    }
}
