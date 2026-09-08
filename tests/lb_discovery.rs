//! Live verification that a real Trader Joe LB `LBPairCreated` log decodes
//! through this crate's `ILBFactory` ABI binding.
//!
//! This is the durable half of the verification recorded in
//! `src/contracts.rs` and `lb_pair_created_topic_matches_deployed_contract`
//! in `src/contracts_rpc.rs`: rather than asserting a hash against a
//! constant derived from the same ABI (which proves nothing), this fetches
//! real logs off Avalanche C-Chain and decodes one through the binding, so a
//! parameter-order or type mistake in `contracts/ABI/ILBFactory.json` would
//! show up as a decode failure here.
//!
//! ```bash
//! cargo test --features collector --test lb_discovery -- --ignored --nocapture
//! ```

#![cfg(feature = "collector")]

mod common;

use alloy::primitives::{address, Address};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::Filter;
use alloy::sol_types::SolEvent;
use anyhow::Result;

use common::RPC_URL;

// Local binding purely to read `LBPairCreated`'s `SIGNATURE_HASH` and decode
// a live log through it. `evm_dex_pool::contracts::ILBFactory` is
// `pub(crate)` and unreachable from this integration-test crate, so it is
// derived here from the same ABI file `src/contracts.rs` uses — exactly the
// pattern `tests/lb_convergence.rs` already uses for `ILBPair`/`ILBPairV20`.
alloy::sol! {
    ILBFactory,
    "contracts/ABI/ILBFactory.json"
}

/// Trader Joe's v2.2 factory on Avalanche C-Chain. Confirmed live: both
/// `getFactory()` on v2.2 pairs and (via this test) `eth_getLogs` against
/// this address for `LBPairCreated` return results — see
/// `src/lb/factories.rs`.
const V22_FACTORY: Address = address!("b43120c4745967fa9b93e79c149e66b0f2d6fe0c");

/// Verified live by `eth_getLogs` against `V22_FACTORY`: pair creation on
/// Avalanche is concentrated in this earlier era, not spread evenly across
/// recent history — sparse sampling elsewhere found nothing, but 2,000-block
/// windows in the 88.7M-91.5M range held 24-97 `LBPairCreated` events each.
/// This exact window is one of them; the test itself prints the count it
/// gets back. If the archive endpoint's retention window ever moves this
/// range out of reach, re-sample a replacement the same way and update this
/// comment.
const RANGE: (u64, u64) = (91_406_504, 91_408_504);

/// The gate this file exists for: `LBPairCreated` is not merely "a topic
/// that doesn't collide with anything else" — a real chain log decodes
/// through the `ILBFactory` `sol!` binding with the exact field layout
/// `LBPairCreated(address indexed tokenX, address indexed tokenY, uint256
/// indexed binStep, address LBPair, uint256 pid)`.
///
/// Deliberately does NOT assert which pair, pid, or token pair comes back —
/// log ordering within a range is not something to depend on, and a
/// re-sample of the same range on a different day could return logs in a
/// different order. What's asserted is the shape: four topics (topic0 plus
/// three indexed params), and a decoded LBPair address / binStep that look
/// like real pair-creation output rather than garbage from a field-order
/// mistake in the ABI.
#[tokio::test]
#[ignore]
async fn lb_pair_created_decodes_from_a_live_log() -> Result<()> {
    let provider = ProviderBuilder::new().connect_http(RPC_URL.parse()?);

    let filter = Filter::new()
        .address(V22_FACTORY)
        .event_signature(ILBFactory::LBPairCreated::SIGNATURE_HASH)
        .from_block(RANGE.0)
        .to_block(RANGE.1);

    let logs = provider.get_logs(&filter).await?;
    println!(
        "[lb_discovery] {} LBPairCreated event(s) from {V22_FACTORY} in {}..={}",
        logs.len(),
        RANGE.0,
        RANGE.1
    );
    assert!(
        !logs.is_empty(),
        "pinned range {RANGE:?} held no LBPairCreated events from {V22_FACTORY} \
         — re-sample a replacement range"
    );

    let log = &logs[0];
    assert_eq!(
        log.topics().len(),
        4,
        "LBPairCreated declares three indexed params, so a log should carry \
         topic0 plus 3 indexed topics, got {}",
        log.topics().len()
    );

    let decoded = log.log_decode::<ILBFactory::LBPairCreated>()?.inner.data;

    assert_ne!(
        decoded.LBPair,
        Address::ZERO,
        "decoded LBPair address must be non-zero"
    );
    assert_ne!(
        decoded.tokenX,
        Address::ZERO,
        "decoded tokenX address must be non-zero"
    );
    assert_ne!(
        decoded.tokenY,
        Address::ZERO,
        "decoded tokenY address must be non-zero"
    );
    assert_ne!(
        decoded.tokenX, decoded.tokenY,
        "a pair cannot be created against itself"
    );

    let bin_step: u64 = decoded.binStep.to();
    assert!(
        bin_step > 0 && bin_step <= 200,
        "decoded binStep {bin_step} is outside the range real LB bin steps use \
         (typically 1-100), suggesting a field-order mismatch in the ABI"
    );

    println!(
        "[lb_discovery] decoded LBPair={} tokenX={} tokenY={} binStep={}",
        decoded.LBPair, decoded.tokenX, decoded.tokenY, bin_step
    );

    Ok(())
}
