//! Event-replay convergence: state built by applying logs from block A to
//! block B must equal state fetched fresh at block B.
//!
//! ```bash
//! cargo test --features collector test_lb_convergence -- --ignored --nocapture
//! ```

#![cfg(feature = "collector")]

mod common;

use alloy::primitives::{address, Address, B256};
use alloy::sol_types::SolEvent;
use anyhow::Result;

use common::converge_one;

// Local bindings purely to read off `SIGNATURE_HASH` constants for the
// coverage assertion below. `evm_dex_pool::contracts::ILBPair` is
// `pub(crate)` and unreachable from this integration-test crate, so the
// topics are derived here from the same ABI file `src/contracts.rs` uses,
// rather than hardcoded.
alloy::sol! {
    ILBPair,
    "contracts/ABI/ILBPair.json"
}

alloy::sol! {
    ILBPairV20,
    "contracts/ABI/ILBPairV20.json"
}

/// Every topic `apply_log` handles and that actually occurs on-chain for
/// v2.1/v2.2 pools. `StaticFeeParametersSet` is deliberately excluded — see
/// `apply_log_updates_static_fee_parameters` in `src/lb/pool.rs` for why no
/// pinned range can cover it.
const REQUIRED_TOPICS: [(&str, B256); 3] = [
    ("Swap", ILBPair::Swap::SIGNATURE_HASH),
    ("DepositedToBins", ILBPair::DepositedToBins::SIGNATURE_HASH),
    (
        "WithdrawnFromBins",
        ILBPair::WithdrawnFromBins::SIGNATURE_HASH,
    ),
];

/// Every topic `apply_log` handles and that actually occurs on-chain for
/// v2.0 pools. Deliberately just two.
///
/// `DepositedToBin` and `CompositionFee` are excluded because they fire
/// **zero** times in the most recent 500,000 blocks on the v2.0 fixture —
/// this pool is deprecated and holders are only exiting it, never adding.
/// Its mint traffic is all from 2022 (blocks ~22.45M), far outside any range
/// this suite would pin, so naming them here would make the coverage
/// assertion fail correctly and permanently. Those two arms are unit-tested
/// instead, in `v20_liquidity_events_update_bins` in `src/lb/pool.rs`.
const V20_REQUIRED_TOPICS: [(&str, B256); 2] = [
    ("Swap", ILBPairV20::Swap::SIGNATURE_HASH),
    (
        "WithdrawnFromBin",
        ILBPairV20::WithdrawnFromBin::SIGNATURE_HASH,
    ),
];

/// Coverage set for the flash-loan fixture. `FlashLoan` is the whole point of
/// the range; `Swap` rides along so the range is not exclusively flash loans
/// and the ordinary replay path is still exercised beside them.
const FLASHLOAN_REQUIRED_TOPICS: [(&str, B256); 2] = [
    ("Swap", ILBPair::Swap::SIGNATURE_HASH),
    ("FlashLoan", ILBPair::FlashLoan::SIGNATURE_HASH),
];

const V22_POOL: Address = address!("8573f98175d816d520248b5facf40d309b1c9cee");
const V21_POOL: Address = address!("4224f6f4c9280509724db2dbac314621e4465c29");
/// LB v2.0 USDC.e/USDC on Avalanche, binStep 1. The only v2.0 pair with
/// enough surviving event traffic to replay.
const V20_POOL: Address = address!("18332988456C4Bd9ABa6698ec748b331516F5A14");

/// v2.1. Verified live by chunked `eth_getLogs` over this exact range: 302
/// logs — 282 Swap, 4 DepositedToBins, 4 WithdrawnFromBins, 4
/// CompositionFees, 8 TransferBatch. Also contains swap gaps both above and
/// below the pool's 30s filterPeriod, so it exercises both branches of
/// update_references. `CompositionFees` needs no handler: each fires in the
/// same transaction as a `DepositedToBins`, which already reports amounts
/// inclusive of the fee — bins matched exactly across all 1423 under `u128`
/// equality with both `CompositionFees` events unhandled. A handler would
/// double-count and turn this passing gate into a failing one.
const V21_RANGE: (u64, u64) = (93_341_000, 93_348_000);

/// v2.2. Verified live by chunked `eth_getLogs` over this exact range: 164
/// logs — 159 Swap, 1 DepositedToBins, 1 WithdrawnFromBins, 1
/// CompositionFees, 2 TransferBatch. Deliberately 11,000 blocks wide: this
/// pool's non-Swap events are sparse enough that a narrow window catches
/// only Swaps, which is how an earlier run "converged" on 9 logs while
/// exercising exactly one code path.
const V22_RANGE: (u64, u64) = (93_271_000, 93_282_000);

/// The regression gate for a pool whose non-Swap event traffic is sparse.
/// Pinned to `V22_RANGE`, verified to contain a `DepositedToBins` and a
/// `WithdrawnFromBins` alongside the Swaps — see its doc comment. A
/// head-derived window over this pool has previously converged on 9 logs
/// that were all Swaps, exercising a single code path while looking like a
/// full pass.
#[tokio::test]
#[ignore]
async fn test_lb_convergence_v22() -> Result<()> {
    converge_one(V22_POOL, "v2.2", Some(V22_RANGE), &REQUIRED_TOPICS).await
}

/// The regression gate for the `update_references`-in-`apply_log` fix.
/// Deliberately pinned to `V21_RANGE` — see its doc comment for why a
/// head-derived range cannot be trusted to exercise both branches of
/// `update_references`. If this range ever needs to move (e.g. the RPC's
/// history retention changes), re-measure a replacement range and update the
/// doc comment on `V21_RANGE` with what it was verified to cover.
#[tokio::test]
#[ignore]
async fn test_lb_convergence_v21() -> Result<()> {
    converge_one(V21_POOL, "v2.1", Some(V21_RANGE), &REQUIRED_TOPICS).await
}

/// v2.1, flash-loan coverage. Verified live by `eth_getLogs` over this exact
/// range: 19 logs — 15 Swap and 4 FlashLoan, and nothing else.
///
/// The four flash loans, decoded from their raw logs:
///
/// ```text
/// block 93501454  activeId 8395265  totalFees (x=1, y=0)  protocolFees (0, 0)
/// block 93501469  activeId 8395265  totalFees (x=1, y=0)  protocolFees (0, 0)
/// block 93501476  activeId 8395263  totalFees (x=1, y=0)  protocolFees (0, 0)
/// block 93502199  activeId 8395264  totalFees (x=1, y=0)  protocolFees (0, 0)
/// ```
///
/// So this range is a real regression gate, not a smoke test: without the
/// `FlashLoan` arm in `apply_log` the replay ends up 2 short on bin 8_395_265
/// and 1 short on each of 8_395_263 and 8_395_264, and
/// `assert_lb_pools_converge` fails on those bins. Block 93,502,199 is the
/// one independently observed moving bin 8,395,264 from 1272 to 1273 while a
/// collector without the fix stayed at 1272.
///
/// What this range does NOT cover: every one of the four has
/// `protocolFees == 0`, so nothing here distinguishes crediting `totalFees`
/// from crediting `totalFees - protocolFees`. That distinction is covered by
/// `flash_loan_credits_the_named_bin_net_of_protocol_fees` in
/// `src/lb/pool.rs`. Likewise the event's `activeId` equals the pool's
/// `active_id` throughout, so the "use the bin the event names" choice is
/// only pinned by that unit test.
const V21_FLASHLOAN_RANGE: (u64, u64) = (93_501_000, 93_503_000);

/// The end-to-end proof for the `FlashLoan` arm: replay across four real
/// flash loans and refetch. Passing means the flash-loan credit, the bins it
/// lands in, and the decision to leave `active_id` / the volatility fields /
/// `time_of_last_update` alone all agree with the chain — `converge_one`
/// compares every one of those fields, so if `flashLoan()` did touch the
/// variable-fee parameters on chain this test would fail rather than pass
/// quietly.
#[tokio::test]
#[ignore]
async fn test_lb_convergence_v21_flashloan() -> Result<()> {
    converge_one(
        V21_POOL,
        "v2.1-flashloan",
        Some(V21_FLASHLOAN_RANGE),
        &FLASHLOAN_REQUIRED_TOPICS,
    )
    .await
}

/// v2.0. Verified live: 2 Swap (blocks 93299096 and 93299113, 17s apart —
/// above the pool's 10s filterPeriod, so `update_references` genuinely
/// fires) and 5 WithdrawnFromBin, plus FeesCollected / TransferSingle /
/// TransferBatch as unhandled bystanders.
///
/// This is the ONLY 2,000-block window in 300,000 containing both a Swap and
/// a WithdrawnFromBin. `DepositedToBin` and `CompositionFee` do not occur at
/// all in the most recent 500,000 blocks — see `v20_liquidity_events_update_bins`
/// in `src/lb/pool.rs` for their coverage.
///
/// Expect this test to take ~40s: v2.0 has no corroborated bin-tree layout,
/// so each of the two fetches walks bins sequentially (~19s for 68 bins).
const V20_RANGE: (u64, u64) = (93_298_348, 93_300_348);

/// The acceptance gate for v2.0 event replay, and the test that adjudicates
/// whether v2.0's `bin += amountIn - amountOut` is right. v2.0's `Swap`
/// reports `amountIn` net of fees where v2.1+ reports it gross, so if v2.0's
/// `getBin()` includes fees the replayed bin will land *below* the fetched
/// one. Nothing else settles this — do not soften the assertion to pass it.
#[tokio::test]
#[ignore]
async fn test_lb_convergence_v20() -> Result<()> {
    converge_one(V20_POOL, "v2.0", Some(V20_RANGE), &V20_REQUIRED_TOPICS).await
}
