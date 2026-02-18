use alloy::sol;

// V2 event contracts (for EventApplicable + TopicList)
sol! {
    IUniswapV2Pair,
    "contracts/ABI/IUniswapV2Pair.json"
}

sol! {
    IV2PairUint256,
    "contracts/ABI/IV2PairUint256.json"
}

// V3 event contracts (for EventApplicable + TopicList)
sol! {
    IUniswapV3Pool,
    "contracts/ABI/IUniswapV3Pool.json"
}

sol! {
    IPancakeV3Pool,
    "contracts/ABI/IPancakeV3Pool.json"
}

sol! {
    IAlgebraPoolSei,
    "contracts/ABI/IAlgebraPoolSei.json"
}

// ERC4626 event contracts
sol! {
    IERC4626,
    "contracts/ABI/IERC4626.json"
}

// Factory event contracts (for POOL_CREATED_TOPICS)
sol! {
    IUniswapV2Factory,
    "contracts/ABI/IUniswapV2Factory.json"
}

sol! {
    IUniswapV3Factory,
    "contracts/ABI/IUniswapV3Factory.json"
}

sol! {
    IAlgebraFactory,
    "contracts/ABI/IAlgebraFactory.json"
}

sol! {
    IVeloPoolFactory,
    "contracts/ABI/IVeloPoolFactory.json"
}
