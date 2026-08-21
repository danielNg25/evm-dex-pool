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

const V22_POOL: Address = address!("8573f98175d816d520248b5facf40d309b1c9cee");
const V21_POOL: Address = address!("4224f6f4c9280509724db2dbac314621e4465c29");

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
