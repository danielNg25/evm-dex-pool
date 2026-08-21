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

    /// The two generations' Swap events must be distinguishable by topic0,
    /// because apply_log dispatches on topic0 alone.
    #[test]
    fn swap_topics_differ_between_generations() {
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
    }
}
