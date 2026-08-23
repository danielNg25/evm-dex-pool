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

// LB event contracts (for EventApplicable + TopicList)
sol! {
    ILBPair,
    "contracts/ABI/ILBPair.json"
}

// LB v2.0 event contracts (distinct ABI from v2.1+)
sol! {
    ILBPairV20,
    "contracts/ABI/ILBPairV20.json"
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

// Trader Joe Liquidity Book factory. `LBPairCreated` is identical across
// LB v2.0, v2.1 and v2.2 — the same five parameters in the same order — so a
// single binding covers every generation and a single topic0 matches them all.
//
// UNVERIFIED AGAINST A LIVE EVENT. Every other LB topic0 in this crate was
// checked against a log pulled off Avalanche; this one could not be. Pair
// creation is rare enough that both the v2.1 and v2.2 factories emitted zero
// logs across 240,000 recent blocks, and the pairs used as fixtures elsewhere
// in this crate were all created before the archive endpoint's 10,000,000-block
// retention window, so their creation logs are unreachable. The topic is
// therefore derived from this ABI by `sol!` — correct by construction if the
// ABI is right — and is NOT corroborated by an observed `LBPairCreated` log.
// Do not add a test that asserts this hash against a constant computed from
// the same ABI; that proves nothing. If a creation event ever lands inside
// the archive window, verify it then and say so here.
sol! {
    ILBFactory,
    "contracts/ABI/ILBFactory.json"
}
