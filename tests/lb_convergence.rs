//! Event-replay convergence: state built by applying logs from block A to
//! block B must equal state fetched fresh at block B.
//!
//! ```bash
//! cargo test --features collector test_lb_convergence -- --ignored --nocapture
//! ```

#![cfg(feature = "collector")]

mod common;

use alloy::primitives::{address, Address};
use anyhow::Result;

use common::converge_one;

const V22_POOL: Address = address!("8573f98175d816d520248b5facf40d309b1c9cee");
const V21_POOL: Address = address!("4224f6f4c9280509724db2dbac314621e4465c29");

/// Pinned deliberately. A head-derived window can contain only tightly-spaced
/// swaps, in which case update_references is a no-op on both sides and the
/// reference assertions pass without testing anything — this test was observed
/// passing against the unfixed code for exactly that reason.
///
/// Re-measured on this range: 61 logs emitted by the pool, of which 56 match
/// `LBPool::topics()` and are applied — 56 is the count this test prints, not
/// 61. Those are 53 Swaps across 34 distinct blocks, with 14 of the 35 swap
/// txs emitting more than one Swap log (up to 3), so replay's repeated
/// `update_references` path is exercised. Inter-swap-block gaps span 1s..383s:
/// 17 are >= the pool's 30s filterPeriod, so update_references fires, and 16
/// are below it, so the non-firing branch is covered too. The range also
/// carries two CompositionFees events, at blocks 93345318 and 93346900.
const V21_PINNED_RANGE: (u64, u64) = (93_345_191, 93_347_191);

/// Live smoke check against current chain traffic for a pool whose
/// `volAcc`/`volRef` stay at 0 throughout typical sampled windows, so it has
/// no reference-update behaviour to pin. Head-derived is fine here: this
/// test is a shape/liveness canary, not a regression gate.
#[tokio::test]
#[ignore]
async fn test_lb_convergence_v22() -> Result<()> {
    converge_one(V22_POOL, "v2.2", None).await
}

/// The regression gate for the `update_references`-in-`apply_log` fix.
/// Deliberately pinned to `V21_PINNED_RANGE` — see its doc comment for why a
/// head-derived range cannot be trusted to exercise both branches of
/// `update_references`. If this range ever needs to move (e.g. the RPC's
/// history retention changes), re-measure a replacement range and update the
/// doc comment on `V21_PINNED_RANGE` with what it was verified to cover.
#[tokio::test]
#[ignore]
async fn test_lb_convergence_v21() -> Result<()> {
    converge_one(V21_POOL, "v2.1", Some(V21_PINNED_RANGE)).await
}

/// Live smoke check against current chain traffic. Not the regression gate —
/// see `test_lb_convergence_v21` (pinned) for that — because a head-derived
/// window can land on a stretch of only tightly-spaced swaps and pass
/// without ever exercising `update_references`'s decay branch.
#[tokio::test]
#[ignore]
async fn test_lb_convergence_v21_head() -> Result<()> {
    converge_one(V21_POOL, "v2.1-head", None).await
}
