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

    #[test]
    fn v21_withdrawn_from_bins_topic_matches_deployed_contract() {
        assert_eq!(
            crate::contracts::ILBPair::WithdrawnFromBins::SIGNATURE_HASH,
            hex!("a32e146844d6144a22e94c586715a1317d58a8aa3581ec33d040113ddcb24350")
        );
    }
}
