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
// VERIFIED AGAINST A LIVE EVENT. The earlier caveat here — that pair creation
// looked too rare to observe within reach of the archive endpoint's
// retention window — turned out to be a sampling artifact: creation is
// concentrated in an earlier era (24-97 `LBPairCreated` logs per 2,000-block
// window around Avalanche blocks 88.7M-91.5M), not spread evenly across
// recent history. A real log, decoded through this exact binding, matched
// `LBPairCreated(address indexed tokenX, address indexed tokenY, uint256
// indexed binStep, address LBPair, uint256 pid)` field-for-field: three
// indexed topics (tokenX, tokenY, binStep) and two data words (LBPair, pid),
// emitted by the v2.2 factory `0xb43120c4745967fa9b93E79C149E66B0f2D6Fe0c`.
// See `lb_pair_created_topic_matches_deployed_contract` in
// `src/contracts_rpc.rs` for the pinned topic0 hash and
// `tests/lb_discovery.rs` for the live fetch-and-decode integration test.
sol! {
    ILBFactory,
    "contracts/ABI/ILBFactory.json"
}
