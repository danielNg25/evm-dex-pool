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
