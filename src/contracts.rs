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

// Ramses-family CL pools (Pharaoh, Shadow, Nile, Cleo) emit this whenever their
// mutable fee changes, carrying both the old and the new value. Verified on
// Avalanche against every fee change in run 6's failing replay blocks (6/6),
// e.g. 0x71bd7525 at block 96081609: FeeAdjustment(800, 5500), topic0
// 0x0cba8718...df24. See `fee_adjustment_topic_matches_deployed_contract`.
//
// Their `Mint` is Uniswap V3's with the NFT position `index` added, so its
// topic0 (0xd78218c0...) is not the Uniswap V3 Mint's and was never fetched:
// run 8 applied every Ramses Burn (unchanged signature) and no Ramses Mint.
// Verified on Avalanche 0xf01449c0 at block 96213057. See
// `ramses_mint_topic_matches_deployed_contract`.
sol! {
    interface IRamsesCLPool {
        event FeeAdjustment(uint24 oldFee, uint24 newFee);
        event Mint(
            address sender,
            address indexed owner,
            uint256 index,
            int24 indexed tickLower,
            int24 indexed tickUpper,
            uint128 amount,
            uint256 amount0,
            uint256 amount1
        );
    }
}

// Algebra Integral pool events that change or report the swap fee (core
// IAlgebraPoolEvents). Verified live: Fee on Avalanche 0x259d… at block
// 96050122 (5000 -> 500); SwapFee beside every Integral swap on both
// Avalanche and Flare; Plugin and PluginConfig on Flare 0x1922… at 70513172.
sol! {
    interface IAlgebraIntegralPool {
        event Fee(uint16 fee);
        event PluginConfig(uint8 newPluginConfig);
        event Plugin(address newPluginAddress);
        event SwapFee(address indexed sender, uint24 overrideFee, uint24 pluginFee);
    }
}
