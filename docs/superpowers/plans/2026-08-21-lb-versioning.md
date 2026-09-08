# LB Version Abstraction & v2.0 Support — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an explicit `LBVersion` abstraction to the existing Trader Joe Liquidity Book implementation and extend it to support LB v2.0, without breaking `evm-dex-pool` 1.4.0 consumers.

**Architecture:** `LBVersion` becomes a field on the existing `LBPool` struct rather than splitting per-version structs, because swap/fee/price math is byte-identical across v2.0/v2.1/v2.2 — only fetching, event decoding, and hooks awareness vary. Version is determined by a positive multicall discriminator (a method only that version has), which replaces today's `getBinStep()` probe and makes the bin-tree storage slot a known value instead of a guess.

**Tech Stack:** Rust 2021, `alloy` 1.7.3 (`sol!` ABI bindings, `MulticallBuilder`), `anyhow`, `serde`, `tokio`. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-08-21-lb-versioning-design.md`

## Global Constraints

- **Branch:** all work on `feature/trader-joe` (currently at `03bb242`, crate version `1.4.0`).
- **Do not modify `src/lb/math.rs`.** Swap, fee, and price math is identical across all three LB versions.
- **`PoolType::TraderJoeLB` stays flat** — never `TraderJoeLB(LBVersion)`. `PoolRegistry::get_pools_by_type` compares by exact equality at `src/registry.rs:135` and `:164`.
- **1.4.0 consumers must keep compiling.** `evm-dex-arbitrage` and `arbitrage-bot-dashboard-be` pin `version = "1.4.0"` and reference `LBPool` / `PoolType::TraderJoeLB` across five files. New struct fields need `#[serde(default)]`; new trait methods need default impls; existing public signatures do not change.
- **New `LBPool` fields go at the END of the struct.** Downstream persists `LBPool` with **bincode** (`evm-dex-arbitrage/src/core/database/mod.rs:47,51`), which is positional and ignores serde defaults. A field inserted mid-struct makes bincode read the *next* field's bytes as the new one — and an `LBVersion` tag of 0/1/2 decodes as valid, silently shifting every later field and corrupting a live pool's reserves. Trailing fields fail cleanly at EOF, which `persistence.rs:98` logs and skips, so the pool is re-fetched. See spec §8.1.
- **ABI JSON files are raw top-level arrays** (`[{...}]`), never Hardhat/Truffle artifact wrappers. All 23 files in `contracts/ABI/` follow this.
- **Binding split:** event-decoding bindings go in `src/contracts.rs`; provider-calling bindings go in `src/contracts_rpc.rs` with `#[sol(rpc)]` and an `Rpc` prefix.
- **Feature gating:** anything touching a `Provider` sits behind `#[cfg(feature = "rpc")]`. `cargo check` with no features must pass.
- **Integration tests hit live RPC and are `#[ignore]`d**, matching `tests/lb_pool.rs` and `tests/collector_add_pools.rs`.
- **Verify both builds after every task:** `cargo check` and `cargo check --features collector`.

## File Structure

| File | Responsibility |
|---|---|
| `src/lb/version.rs` *(new)* | `LBVersion` enum; `detect_lb_version` RPC discriminator (rpc-gated) |
| `src/lb/pool.rs` | `LBPool` struct + fields; `apply_log` for both event generations |
| `src/lb/fetcher.rs` | Version-aware state fetch and bin discovery |
| `src/lb/mod.rs` | Re-export `LBVersion` |
| `contracts/ABI/ILBPair.json` | v2.1/v2.2 ABI — add `getLBHooksParameters`, `CompositionFees` |
| `contracts/ABI/ILBPairV20.json` *(new)* | v2.0 ABI |
| `src/contracts.rs` / `src/contracts_rpc.rs` | `ILBPairV20` / `RpcILBPairV20` bindings |
| `src/pool/base.rs` | `QuoteContext`; defaulted `calculate_output_at` / `calculate_input_at` |
| `src/collector/pool_fetcher.rs` | `identify_pool_type` rewire |
| `tests/lb_convergence.rs` *(new)* | Event-replay convergence test |
| `tests/lb_pool.rs` | Extend quote-parity fuzz to v2.0 |

---

### Task 1: `LBVersion` type and `LBPool` fields

**Files:**
- Create: `src/lb/version.rs`
- Modify: `src/lb/mod.rs`, `src/lb/pool.rs`
- Test: inline `mod tests` in `src/lb/version.rs`

**Interfaces:**
- Produces: `pub enum LBVersion { V2_0, V2_1, V2_2 }` with `Default → V2_1`; `LBPool::version: LBVersion`; `LBPool::hooks_parameters: Option<B256>`; builder methods `LBPool::with_version(self, LBVersion) -> Self` and `LBPool::with_hooks_parameters(self, Option<B256>) -> Self`.

- [ ] **Step 1: Write the failing test**

Create `src/lb/version.rs` containing only the test module for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_v21() {
        // Snapshots persisted by evm-dex-pool 1.4.0 predate v2.0 support,
        // so an absent version must deserialize as v2.1.
        assert_eq!(LBVersion::default(), LBVersion::V2_1);
    }

    #[test]
    fn round_trips_through_serde() {
        for v in [LBVersion::V2_0, LBVersion::V2_1, LBVersion::V2_2] {
            let json = serde_json::to_string(&v).unwrap();
            let back: LBVersion = serde_json::from_str(&json).unwrap();
            assert_eq!(v, back);
        }
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib lb::version`
Expected: FAIL — `cannot find type LBVersion in this scope`.

- [ ] **Step 3: Write minimal implementation**

Prepend to `src/lb/version.rs`:

```rust
//! LB protocol version and runtime detection.

use serde::{Deserialize, Serialize};

/// Trader Joe Liquidity Book protocol generation.
///
/// Swap, fee, and price math is identical across all three. Version affects
/// only state fetching, event decoding, and hooks awareness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LBVersion {
    /// Original LB. Distinct getters and event ABIs from v2.1+.
    V2_0,
    /// Bin tree at storage slot 8 (ReentrancyGuard `_status` shifts slots by 1).
    V2_1,
    /// Bin tree at storage slot 7. Adds hooks. Event ABIs identical to v2.1.
    V2_2,
}

impl Default for LBVersion {
    fn default() -> Self {
        Self::V2_1
    }
}
```

Add to `src/lb/mod.rs`, after the existing `pub mod math;` line:

```rust
pub mod version;
pub use version::LBVersion;
```

Add `serde_json` to `[dev-dependencies]` in `Cargo.toml` if not already present:

```toml
serde_json = "1.0"
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib lb::version`
Expected: PASS — 2 tests.

- [ ] **Step 5: Add the fields to `LBPool`**

In `src/lb/pool.rs`, add to the imports:

```rust
use crate::lb::version::LBVersion;
use alloy::primitives::B256;
```

In the `LBPool` struct, immediately after `pub address: Address,`:

```rust
    /// LB protocol generation. Defaults to v2.1 for snapshots persisted
    /// before v2.0 support existed.
    #[serde(default)]
    pub version: LBVersion,
```

At the end of the struct, after `pub created_at: u64,`:

```rust
    /// v2.2 only. `Some(non-zero)` means the pair has hooks installed and its
    /// observable behaviour may deviate from pure LB math.
    #[serde(default)]
    pub hooks_parameters: Option<B256>,
```

In `LBPool::new`, add both fields to the constructed `Self { .. }` literal using defaults, so the 17-argument signature is unchanged and every existing caller keeps compiling:

```rust
            version: LBVersion::default(),
            hooks_parameters: None,
```

Then add builder methods inside `impl LBPool`, directly after `new`:

```rust
    /// Set the protocol version. Chainable after `new`.
    pub fn with_version(mut self, version: LBVersion) -> Self {
        self.version = version;
        self
    }

    /// Set the v2.2 hooks parameters. Chainable after `new`.
    pub fn with_hooks_parameters(mut self, hooks: Option<B256>) -> Self {
        self.hooks_parameters = hooks;
        self
    }
```

- [ ] **Step 6: Write the backward-compatibility test**

Append to the `mod tests` in `src/lb/version.rs`:

```rust
    use crate::lb::LBPool;
    use alloy::primitives::Address;
    use std::collections::BTreeMap;

    #[test]
    fn pool_without_version_field_deserializes_as_v21() {
        // Build a pool, serialize it, then strip `version` to simulate a
        // snapshot written by evm-dex-pool 1.4.0.
        let pool = LBPool::new(
            Address::ZERO, Address::ZERO, Address::ZERO,
            20, 8_388_608, BTreeMap::new(),
            5_000, 30, 600, 5_000, 40_000, 1_000, 350_000,
            0, 0, 8_388_608, 0,
        );
        let mut value: serde_json::Value = serde_json::to_value(&pool).unwrap();
        value.as_object_mut().unwrap().remove("version");
        value.as_object_mut().unwrap().remove("hooks_parameters");

        let restored: LBPool = serde_json::from_value(value).unwrap();
        assert_eq!(restored.version, LBVersion::V2_1);
        assert_eq!(restored.hooks_parameters, None);
    }
```

- [ ] **Step 7: Run tests to verify they pass**

Run: `cargo test --lib lb::version`
Expected: PASS — 3 tests.

- [ ] **Step 8: Verify both builds**

Run: `cargo check && cargo check --features collector`
Expected: both succeed with no new warnings.

- [ ] **Step 9: Commit**

```bash
git add src/lb/version.rs src/lb/mod.rs src/lb/pool.rs Cargo.toml
git commit -m "feat(lb): add LBVersion and version/hooks fields to LBPool

Fields are additive with serde defaults so snapshots persisted by 1.4.0
still deserialize. LBPool::new keeps its signature; version is set via
the chainable with_version."
```

---

### Task 2: v2.0 ABI and contract bindings

**Files:**
- Create: `contracts/ABI/ILBPairV20.json`
- Modify: `contracts/ABI/ILBPair.json`, `src/contracts.rs`, `src/contracts_rpc.rs`
- Test: inline `mod tests` in `src/contracts_rpc.rs`

**Interfaces:**
- Produces: `crate::contracts::ILBPairV20` (event decoding), `crate::contracts_rpc::RpcILBPairV20` (provider calls). `ILBPair` gains `getLBHooksParameters()` and the `CompositionFees` event.

The test validates the hand-written ABI against **function selectors observed on deployed contracts**, so a wrong parameter type is caught offline rather than at runtime.

- [ ] **Step 1: Write the failing test**

Append to `src/contracts_rpc.rs`:

```rust
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
    fn swap_topics_match_deployed_contracts() {
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --features collector --lib contracts_rpc`
Expected: FAIL — `RpcILBPairV20` not found.

- [ ] **Step 3: Create the v2.0 ABI**

Create `contracts/ABI/ILBPairV20.json` as a raw top-level array:

```json
[
  { "type": "function", "name": "tokenX", "inputs": [], "outputs": [{ "name": "", "type": "address" }], "stateMutability": "view" },
  { "type": "function", "name": "tokenY", "inputs": [], "outputs": [{ "name": "", "type": "address" }], "stateMutability": "view" },
  { "type": "function", "name": "factory", "inputs": [], "outputs": [{ "name": "", "type": "address" }], "stateMutability": "view" },
  { "type": "function", "name": "getReservesAndId", "inputs": [], "outputs": [
      { "name": "reserveX", "type": "uint256" },
      { "name": "reserveY", "type": "uint256" },
      { "name": "activeId", "type": "uint256" }
    ], "stateMutability": "view" },
  { "type": "function", "name": "getBin", "inputs": [{ "name": "id", "type": "uint24" }], "outputs": [
      { "name": "reserveX", "type": "uint256" },
      { "name": "reserveY", "type": "uint256" }
    ], "stateMutability": "view" },
  { "type": "function", "name": "findFirstNonEmptyBinId", "inputs": [
      { "name": "id_", "type": "uint24" },
      { "name": "sentTokenY", "type": "bool" }
    ], "outputs": [{ "name": "id", "type": "uint24" }], "stateMutability": "view" },
  { "type": "function", "name": "feeParameters", "inputs": [], "outputs": [
      { "name": "", "type": "tuple", "components": [
        { "name": "binStep", "type": "uint16" },
        { "name": "baseFactor", "type": "uint16" },
        { "name": "filterPeriod", "type": "uint16" },
        { "name": "decayPeriod", "type": "uint16" },
        { "name": "reductionFactor", "type": "uint16" },
        { "name": "variableFeeControl", "type": "uint24" },
        { "name": "protocolShare", "type": "uint16" },
        { "name": "maxVolatilityAccumulated", "type": "uint24" },
        { "name": "volatilityAccumulated", "type": "uint24" },
        { "name": "volatilityReference", "type": "uint24" },
        { "name": "indexRef", "type": "uint24" },
        { "name": "time", "type": "uint40" }
      ]}
    ], "stateMutability": "view" },
  { "type": "event", "name": "Swap", "anonymous": false, "inputs": [
      { "name": "sender", "type": "address", "indexed": true },
      { "name": "recipient", "type": "address", "indexed": true },
      { "name": "id", "type": "uint256", "indexed": true },
      { "name": "swapForY", "type": "bool", "indexed": false },
      { "name": "amountIn", "type": "uint256", "indexed": false },
      { "name": "amountOut", "type": "uint256", "indexed": false },
      { "name": "volatilityAccumulated", "type": "uint256", "indexed": false },
      { "name": "fees", "type": "uint256", "indexed": false }
    ]},
  { "type": "event", "name": "DepositedToBin", "anonymous": false, "inputs": [
      { "name": "sender", "type": "address", "indexed": true },
      { "name": "recipient", "type": "address", "indexed": true },
      { "name": "id", "type": "uint256", "indexed": true },
      { "name": "amountX", "type": "uint256", "indexed": false },
      { "name": "amountY", "type": "uint256", "indexed": false }
    ]},
  { "type": "event", "name": "WithdrawnFromBin", "anonymous": false, "inputs": [
      { "name": "sender", "type": "address", "indexed": true },
      { "name": "recipient", "type": "address", "indexed": true },
      { "name": "id", "type": "uint256", "indexed": true },
      { "name": "amountX", "type": "uint256", "indexed": false },
      { "name": "amountY", "type": "uint256", "indexed": false }
    ]},
  { "type": "event", "name": "CompositionFee", "anonymous": false, "inputs": [
      { "name": "sender", "type": "address", "indexed": true },
      { "name": "recipient", "type": "address", "indexed": true },
      { "name": "id", "type": "uint256", "indexed": true },
      { "name": "feesX", "type": "uint256", "indexed": false },
      { "name": "feesY", "type": "uint256", "indexed": false }
    ]}
]
```

- [ ] **Step 4: Extend the v2.1+ ABI**

Add two entries to the array in `contracts/ABI/ILBPair.json`:

```json
  { "type": "function", "name": "getLBHooksParameters", "inputs": [], "outputs": [{ "name": "", "type": "bytes32" }], "stateMutability": "view" },
  { "type": "event", "name": "CompositionFees", "anonymous": false, "inputs": [
      { "name": "sender", "type": "address", "indexed": true },
      { "name": "id", "type": "uint24", "indexed": false },
      { "name": "totalFees", "type": "bytes32", "indexed": false },
      { "name": "protocolFees", "type": "bytes32", "indexed": false }
    ]}
```

- [ ] **Step 5: Add the bindings**

Append to `src/contracts.rs`:

```rust
// LB v2.0 event contracts (distinct ABI from v2.1+)
sol! {
    ILBPairV20,
    "contracts/ABI/ILBPairV20.json"
}
```

Append to `src/contracts_rpc.rs`, above the new `mod tests`:

```rust
sol! {
    #[sol(rpc)]
    RpcILBPairV20,
    "contracts/ABI/ILBPairV20.json"
}
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test --features collector --lib contracts_rpc`
Expected: PASS — 4 tests.

If `v20_selectors_match_deployed_contracts` fails, the ABI parameter types are wrong — fix the JSON to match the selector, do not change the expected selector. The selectors were read from deployed contracts and are ground truth.

- [ ] **Step 7: Verify both builds**

Run: `cargo check && cargo check --features collector`
Expected: both succeed.

- [ ] **Step 8: Commit**

```bash
git add contracts/ABI/ILBPairV20.json contracts/ABI/ILBPair.json src/contracts.rs src/contracts_rpc.rs
git commit -m "feat(lb): add v2.0 ABI bindings and missing v2.1 entries

Adds ILBPairV20/RpcILBPairV20 for the v2.0 interface, plus
getLBHooksParameters and the CompositionFees event that were missing
from ILBPair.json. Selectors are asserted against values observed on
deployed pairs so wrong parameter types fail at build time."
```

---

### Task 3: Version detection

**Files:**
- Modify: `src/lb/version.rs`, `src/collector/pool_fetcher.rs`
- Test: `tests/lb_version_detect.rs` *(new)*

**Interfaces:**
- Consumes: `LBVersion` (Task 1); `RpcILBPair`, `RpcILBPairV20` (Task 2).
- Produces: `pub async fn detect_lb_version<P: Provider + Send + Sync>(provider: &Arc<P>, pool_address: Address, multicall_address: Address, block_number: BlockId) -> Result<Option<LBVersion>>` — `Ok(None)` means "not an LB pair".

This replaces the `getBinStep()` probe in `identify_pool_type`, which today misclassifies every v2.0 pair as `UniswapV2` and then panics the bootstrap when `fetch_v2_pool` calls `token0()`.

- [ ] **Step 1: Write the failing test**

Create `tests/lb_version_detect.rs`:

```rust
//! Live version-detection tests against known Avalanche LB pairs.
//!
//! ```bash
//! cargo test --features collector test_detect -- --ignored --nocapture
//! ```

#![cfg(feature = "collector")]

use std::sync::Arc;

use alloy::eips::BlockId;
use alloy::primitives::{address, Address};
use alloy::providers::ProviderBuilder;
use anyhow::Result;

use evm_dex_pool::collector::identify_pool_type;
use evm_dex_pool::lb::{detect_lb_version, LBVersion};
use evm_dex_pool::PoolType;

const RPC_URL: &str = "https://api.avax.network/ext/bc/C/rpc";
const MULTICALL: Address = address!("cA11bde05977b3631167028862bE2a173976CA11");

const V22_POOL: Address = address!("8573f98175d816d520248b5facf40d309b1c9cee");
const V21_POOL: Address = address!("4224f6f4c9280509724db2dbac314621e4465c29");
// LB v2.0 WAVAX/USDC on Avalanche.
const V20_POOL: Address = address!("18332988456C4Bd9ABa6698ec748b331516F5A14");
// A non-LB pair, to prove detection returns None rather than guessing.
const V2_PAIR: Address = address!("f4003F4efBE8691B60249E6afbD307aBE7758adb");

#[tokio::test]
#[ignore]
async fn test_detect_lb_version() -> Result<()> {
    let provider = Arc::new(ProviderBuilder::new().connect_http(RPC_URL.parse()?));
    let block = BlockId::latest();

    for (addr, expected) in [
        (V22_POOL, Some(LBVersion::V2_2)),
        (V21_POOL, Some(LBVersion::V2_1)),
        (V20_POOL, Some(LBVersion::V2_0)),
        (V2_PAIR, None),
    ] {
        let got = detect_lb_version(&provider, addr, MULTICALL, block).await?;
        assert_eq!(got, expected, "version mismatch for {addr}");
    }
    Ok(())
}

/// The regression this whole task exists for: a v2.0 pair must not be
/// classified as UniswapV2, which would panic the bootstrap.
#[tokio::test]
#[ignore]
async fn test_v20_pair_is_not_misclassified() -> Result<()> {
    let provider = Arc::new(ProviderBuilder::new().connect_http(RPC_URL.parse()?));
    let got = identify_pool_type(&provider, V20_POOL, MULTICALL).await?;
    assert_eq!(got, PoolType::TraderJoeLB);
    Ok(())
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --features collector test_detect -- --ignored --nocapture`
Expected: FAIL to compile — `detect_lb_version` not found.

- [ ] **Step 3: Implement detection**

Append to `src/lb/version.rs`:

```rust
#[cfg(feature = "rpc")]
mod detect {
    use super::LBVersion;
    use crate::contracts_rpc::{RpcILBPair, RpcILBPairV20};
    use alloy::eips::BlockId;
    use alloy::primitives::Address;
    use alloy::providers::Provider;
    use anyhow::Result;
    use std::sync::Arc;

    /// Determine which LB generation a pair is, using a positive
    /// discriminator: each version is identified by a method only that
    /// version exposes.
    ///
    /// - `getLBHooksParameters()` → v2.2 only
    /// - `getFactory()` → v2.1 or v2.2
    /// - `factory()` + `feeParameters()` → v2.0
    ///
    /// `factory()` is paired with `feeParameters()` because `factory()` alone
    /// is too common among non-LB contracts to be a reliable signal.
    ///
    /// Returns `Ok(None)` when the address is not an LB pair.
    pub async fn detect_lb_version<P: Provider + Send + Sync>(
        provider: &Arc<P>,
        pool_address: Address,
        multicall_address: Address,
        block_number: BlockId,
    ) -> Result<Option<LBVersion>> {
        let v21 = RpcILBPair::new(pool_address, provider);
        let v20 = RpcILBPairV20::new(pool_address, provider);

        let r = provider
            .multicall()
            .address(multicall_address)
            .add(v21.getLBHooksParameters()) // 0
            .add(v21.getFactory()) // 1
            .add(v20.factory()) // 2
            .add(v20.feeParameters()) // 3
            .block(block_number)
            .try_aggregate(false)
            .await?;

        if r.0.is_ok() {
            return Ok(Some(LBVersion::V2_2));
        }
        if r.1.is_ok() {
            return Ok(Some(LBVersion::V2_1));
        }
        if r.2.is_ok() && r.3.is_ok() {
            return Ok(Some(LBVersion::V2_0));
        }
        Ok(None)
    }
}

#[cfg(feature = "rpc")]
pub use detect::detect_lb_version;
```

Export it from `src/lb/mod.rs` by replacing the `pub use version::LBVersion;` line:

```rust
pub use version::LBVersion;
#[cfg(feature = "rpc")]
pub use version::detect_lb_version;
```

- [ ] **Step 4: Rewire `identify_pool_type`**

In `src/collector/pool_fetcher.rs`, replace the body of `identify_pool_type` (the `getBinStep()` probe) with:

```rust
pub async fn identify_pool_type<P: Provider + Send + Sync>(
    provider: &Arc<P>,
    pool_address: Address,
    multicall_address: Address,
) -> Result<PoolType> {
    // LB first, using a positive per-version discriminator. The previous
    // getBinStep() probe existed only on v2.1+, so v2.0 pairs fell through
    // to the UniswapV2 default and panicked the bootstrap in fetch_v2_pool.
    if crate::lb::detect_lb_version(provider, pool_address, multicall_address, BlockId::latest())
        .await?
        .is_some()
    {
        return Ok(PoolType::TraderJoeLB);
    }

    let v3_instance = IUniswapV3Pool::new(pool_address, provider);
    let result = provider
        .multicall()
        .address(multicall_address)
        .add(v3_instance.liquidity())
        .try_aggregate(false)
        .await?;

    if result.0.is_ok() {
        return Ok(PoolType::UniswapV3);
    }
    Ok(PoolType::UniswapV2)
}
```

Add `use alloy::eips::BlockId;` to the imports if it is not already there.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --features collector test_detect -- --ignored --nocapture`
Expected: PASS — 2 tests. Both hit live Avalanche RPC.

If `V2_PAIR` returns `Some(..)`, the discriminator is too loose — tighten it, do not relax the assertion.

- [ ] **Step 6: Verify both builds**

Run: `cargo check && cargo check --features collector`
Expected: both succeed.

- [ ] **Step 7: Commit**

```bash
git add src/lb/version.rs src/lb/mod.rs src/collector/pool_fetcher.rs tests/lb_version_detect.rs
git commit -m "fix(lb): detect LB version with a positive discriminator

identify_pool_type probed getBinStep(), which only exists on v2.1+, so a
v2.0 pair was classified UniswapV2 and then panicked the bootstrap when
fetch_v2_pool called token0() on it. Detection now probes a method unique
to each generation and returns the exact version."
```

---

### Task 4: Version-aware fetch for v2.1/v2.2

**Files:**
- Modify: `src/lb/fetcher.rs`
- Test: `tests/lb_version_detect.rs` (extend)

**Interfaces:**
- Consumes: `detect_lb_version` (Task 3); `LBPool::with_version` / `with_hooks_parameters` (Task 1).
- Produces: `fetch_lb_pool` keeps its existing 6-argument signature but now stamps `version` and `hooks_parameters` on the returned pool, and selects the bin-tree storage slot deterministically.

Removes the "try slot 7, then slot 8, then walk, then give up and return one bin" chain. The last step is the dangerous one: it yields a pool that quotes successfully while pricing a single bin.

- [ ] **Step 1: Write the failing test**

Append to `tests/lb_version_detect.rs`:

```rust
use evm_dex_pool::lb::fetch_lb_pool;
use evm_dex_pool::TokenInfo;
use std::collections::HashMap;
use std::future::Future;
use std::sync::Mutex;

struct NoopTokenInfo;

impl TokenInfo for NoopTokenInfo {
    fn get_or_fetch_token<P: alloy::providers::Provider + Send + Sync>(
        &self,
        _provider: &Arc<P>,
        address: Address,
        _multicall_address: Address,
    ) -> impl Future<Output = Result<(Address, u8)>> + Send {
        async move { Ok((address, 18u8)) }
    }
}

#[tokio::test]
#[ignore]
async fn test_fetch_stamps_version_and_finds_bins() -> Result<()> {
    let provider = Arc::new(ProviderBuilder::new().connect_http(RPC_URL.parse()?));
    let block = BlockId::latest();

    for (addr, expected) in [(V22_POOL, LBVersion::V2_2), (V21_POOL, LBVersion::V2_1)] {
        let pool =
            fetch_lb_pool(&provider, addr, block, &NoopTokenInfo, MULTICALL, 43114).await?;

        assert_eq!(pool.version, expected, "version not stamped for {addr}");
        // A live WAVAX pair always has far more than one non-empty bin. One
        // bin means bin discovery silently degraded to the walk fallback.
        assert!(
            pool.bins.len() > 1,
            "{addr}: only {} bin(s) discovered — discovery degraded",
            pool.bins.len()
        );
    }
    Ok(())
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --features collector test_fetch_stamps -- --ignored --nocapture`
Expected: FAIL — `assertion failed: pool.version == expected` (fetch currently leaves the default `V2_1`, so the v2.2 case fails).

- [ ] **Step 3: Make bin discovery version-driven**

In `src/lb/fetcher.rs`, add to the imports:

```rust
use crate::lb::{detect_lb_version, LBVersion};
```

Add a helper above `fetch_lb_pool`:

```rust
/// Storage slot of `_tree.level0` for a given LB version.
///
/// v2.1 carries a `uint256 private _status` from ReentrancyGuard that shifts
/// every subsequent slot by one; v2.2 uses ERC-7201 namespaced storage and
/// has no such slot.
fn tree_base_slot(version: LBVersion) -> Option<u64> {
    match version {
        LBVersion::V2_2 => Some(LB_TREE_BASE_SLOT_V22),
        LBVersion::V2_1 => Some(LB_TREE_BASE_SLOT_V21),
        // v2.0's bin tree is a different on-chain structure whose layout is
        // not corroborated; it uses the findFirstNonEmptyBinId walk instead.
        LBVersion::V2_0 => None,
    }
}
```

Replace the entire `let bin_ids = 'discover: { ... };` block with:

```rust
    // ── Batch 2: Discover non-empty bins ─────────────────────────────────
    let bin_ids = match tree_base_slot(version) {
        Some(base) => {
            let ids = discover_bins_from_tree(provider, pool_address, block_number, base)
                .await
                .map_err(|e| {
                    anyhow::anyhow!(
                        "LB bin tree read failed for {} ({:?}, slot {}): {}",
                        pool_address, version, base, e
                    )
                })?;
            if ids.is_empty() {
                return Err(anyhow::anyhow!(
                    "LB bin tree at slot {} for {} ({:?}) yielded no bins — \
                     storage layout may have changed",
                    base, pool_address, version
                ));
            }
            info!(
                "[Chain {}] LB tree bitmap ({:?}, slot {}): discovered {} non-empty bins",
                chain_id, version, base, ids.len()
            );
            ids
        }
        None => {
            let ids =
                discover_bins_by_walking(provider, pool_address, active_id, block_number).await?;
            if ids.is_empty() {
                return Err(anyhow::anyhow!(
                    "LB bin walk for {} ({:?}) yielded no bins",
                    pool_address, version
                ));
            }
            info!(
                "[Chain {}] LB bin walk ({:?}): discovered {} non-empty bins",
                chain_id, version, ids.len()
            );
            ids
        }
    };
```

The previous fallback returned `vec![active_id]` on total failure. Returning an error instead matches the V3 convention where an unfillable swap is `Err`, never a partial answer.

- [ ] **Step 4: Detect the version and stamp it**

At the top of `fetch_lb_pool`, immediately after the `info!` call and before `let lb_instance = ...`:

```rust
    let version = detect_lb_version(provider, pool_address, multicall_address, block_number)
        .await?
        .ok_or_else(|| anyhow::anyhow!("{} is not a Trader Joe LB pair", pool_address))?;
    info!("[Chain {}] LB pool {} detected as {:?}", chain_id, pool_address, version);
```

Fetch the hooks parameters for v2.2, immediately after the existing Batch 1 multicall unpacking:

```rust
    let hooks_parameters = if version == LBVersion::V2_2 {
        lb_instance
            .getLBHooksParameters()
            .block(block_number)
            .call()
            .await
            .ok()
            .filter(|h| !h.is_zero())
    } else {
        None
    };
    if let Some(h) = hooks_parameters {
        warn!(
            "[Chain {}] LB pool {} has hooks installed ({}); observed behaviour \
             may deviate from pure LB math",
            chain_id, pool_address, h
        );
    }
```

Add `warn` to the `log` import: `use log::{info, warn};`

Finally, change the tail of the function from `Ok(pool)` to stamp both fields:

```rust
    Ok(pool
        .with_version(version)
        .with_hooks_parameters(hooks_parameters))
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --features collector test_fetch_stamps -- --ignored --nocapture`
Expected: PASS — both pools report the right version and more than one bin.

- [ ] **Step 6: Verify no regression in existing LB tests**

Run: `cargo test --features collector test_lb_fuzz -- --ignored --nocapture`
Expected: PASS — quote parity is unchanged by this task.

- [ ] **Step 7: Verify both builds**

Run: `cargo check && cargo check --features collector`
Expected: both succeed.

- [ ] **Step 8: Commit**

```bash
git add src/lb/fetcher.rs tests/lb_version_detect.rs
git commit -m "feat(lb): select bin tree slot from detected version

Replaces the try-slot-7-then-8-then-walk chain with a deterministic slot
chosen from the detected version. Bin discovery failure is now an error
rather than a silent degradation to vec![active_id], which produced a
pool that quoted successfully while pricing a single bin."
```

---

### Task 5: Block timestamp in `apply_log`

**Files:**
- Modify: `src/lb/pool.rs`
- Test: inline `mod tests` in `src/lb/pool.rs`

**Interfaces:**
- Produces: `apply_log` uses `Log::block_timestamp` when present, falling back to wall clock only when absent. No signature change — `EventApplicable::apply_log` already receives the full `alloy::rpc::types::Log`.

This is a prerequisite for Task 7. `fetch` seeds `time_of_last_update` from the chain's `getVariableFeeParameters().timeOfLastUpdate`, while `apply_log` overwrites it with `chrono::Utc::now()`. Since `update_references` decays volatility on `dt = timestamp - time_of_last_update` (`src/lb/pool.rs:139`), replayed and fetched state can never agree — and the error feeds straight into the fee and every quote.

- [ ] **Step 1: Write the failing test**

Append to `src/lb/pool.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::ILBPair;
    use alloy::primitives::{Address, LogData, B256, U256};
    use alloy::rpc::types::Log;
    use alloy::sol_types::SolEvent;

    fn pool_with_time(t: u64) -> LBPool {
        let mut p = LBPool::new(
            Address::ZERO, Address::ZERO, Address::ZERO,
            20, 8_388_608, BTreeMap::new(),
            5_000, 30, 600, 5_000, 40_000, 1_000, 350_000,
            0, 0, 8_388_608, t,
        );
        p.update_bin(8_388_608, 1_000_000, 1_000_000);
        p
    }

    /// Build a v2.1 Swap log with a known block timestamp.
    fn swap_log(block_timestamp: Option<u64>) -> Log {
        let event = ILBPair::Swap {
            sender: Address::ZERO,
            to: Address::ZERO,
            id: 8_388_608u32.try_into().unwrap(),
            amountsIn: B256::ZERO,
            amountsOut: B256::ZERO,
            volatilityAccumulator: 0u32.try_into().unwrap(),
            totalFees: B256::ZERO,
            protocolFees: B256::ZERO,
        };
        Log {
            inner: alloy::primitives::Log {
                address: Address::ZERO,
                data: LogData::new_unchecked(event.encode_topics_array::<3>().to_vec(), event.encode_data().into()),
            },
            block_timestamp,
            ..Default::default()
        }
    }

    #[test]
    fn apply_log_uses_block_timestamp_not_wall_clock() {
        let mut pool = pool_with_time(1_700_000_000);
        pool.apply_log(&swap_log(Some(1_700_000_500))).unwrap();
        assert_eq!(
            pool.time_of_last_update, 1_700_000_500,
            "apply_log must take chain time from the log, not the wall clock"
        );
    }

    #[test]
    fn apply_log_falls_back_to_wall_clock_when_log_has_no_timestamp() {
        let mut pool = pool_with_time(1_700_000_000);
        pool.apply_log(&swap_log(None)).unwrap();
        let now = chrono::Utc::now().timestamp() as u64;
        assert!(
            pool.time_of_last_update.abs_diff(now) < 60,
            "expected a wall-clock fallback near now, got {}",
            pool.time_of_last_update
        );
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib lb::pool`
Expected: FAIL on `apply_log_uses_block_timestamp_not_wall_clock` — `time_of_last_update` holds today's wall-clock epoch instead of `1700000500`.

- [ ] **Step 3: Write the implementation**

Add a helper inside `impl LBPool`:

```rust
    /// Chain time for an event, falling back to wall clock only when the log
    /// carries no block timestamp (some RPCs omit it on pending logs).
    fn log_timestamp(event: &Log) -> u64 {
        event
            .block_timestamp
            .unwrap_or_else(|| chrono::Utc::now().timestamp() as u64)
    }
```

In `apply_log`, replace every `let now = chrono::Utc::now().timestamp() as u64;` and every inline `chrono::Utc::now().timestamp() as u64` with:

```rust
        let now = Self::log_timestamp(event);
```

**Locate these by pattern, not by line number** — earlier tasks have already
shifted `src/lb/pool.rs` and will shift it again. Run
`grep -n "chrono::Utc::now()" src/lb/pool.rs` and `grep -n "fn apply_log"` first,
then classify each hit by which function encloses it.

There are exactly **three** in-scope sites, all inside `apply_log`: the `now`
binding in the `Swap` arm, and the `self.last_updated = …` assignments in the
`DepositedToBins` and `WithdrawnFromBins` arms. The `StaticFeeParametersSet` arm
sets no timestamp and needs no change.

**Leave every `chrono::Utc::now()` outside `apply_log` alone.** They are
legitimate wall-clock uses, not bugs:

| Enclosing function | Why wall clock is correct |
|---|---|
| `LBPool::new` | `created_at` / `last_updated` are local bookkeeping |
| `simulate_swap_out` | the deliberately timeless convenience wrapper |
| `simulate_swap_in` | same |
| `apply_swap` (`PoolInterface`) | mutates after a simulated swap, not driven by a log |

As of this writing the in-scope hits are at lines 534, 550, and 563, and the
do-not-touch hits at 90, 208, 308, and 444 — but verify with grep rather than
trusting those numbers.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib lb::pool`
Expected: PASS — 2 tests.

- [ ] **Step 5: Verify both builds**

Run: `cargo check && cargo check --features collector`
Expected: both succeed.

- [ ] **Step 6: Commit**

```bash
git add src/lb/pool.rs
git commit -m "fix(lb): stamp chain time from the log, not the wall clock

apply_log wrote chrono::Utc::now() into time_of_last_update while fetch
seeded it from getVariableFeeParameters().timeOfLastUpdate. Since
update_references decays volatility on the difference, the mismatch
corrupted the variable fee and made replayed state unable to converge
with fetched state."
```

---

### Task 5b: Stamp block timestamps onto collector logs

**Files:**
- Modify: `src/collector/utils.rs`, `src/collector/block_source.rs`, `src/collector/mod.rs`
- Test: `tests/lb_convergence.rs` uses it (Task 7); unit test in `src/collector/utils.rs`

**Interfaces:**
- Produces: `pub async fn enrich_log_timestamps<P: Provider + Send + Sync>(provider: &Arc<P>, logs: &mut [Log]) -> Result<()>`, re-exported from `crate::collector`.

**Why this task exists.** Task 5 made `apply_log` read `Log::block_timestamp`,
falling back to wall clock when absent. Measurement then showed the fallback is
the *only* branch ever taken: **`blockTimestamp` is not part of a standard
`eth_getLogs` response, and Avalanche's endpoint omits it entirely.** Confirmed
by inspecting a live response — the returned keys are `address`, `blockHash`,
`blockNumber`, `data`, `logIndex`, `removed`, `topics`, `transactionHash`,
`transactionIndex`, and nothing else.

Neither collector path repairs this: `src/collector/utils.rs` `fetch_events`
hands `provider.get_logs(&filter)` output straight through, and
`src/collector/websocket_listener.rs` does the same with `subscribe_logs`. So
without this task, Task 5's fix is inert in production, `time_of_last_update`
keeps drifting to wall clock, and **Task 7's convergence test cannot pass.**

- [ ] **Step 1: Write the failing test**

Add to `src/collector/utils.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{Address, LogData};

    fn log_at(block: u64) -> Log {
        Log {
            inner: alloy::primitives::Log {
                address: Address::ZERO,
                data: LogData::new_unchecked(vec![], Default::default()),
            },
            block_number: Some(block),
            block_timestamp: None,
            ..Default::default()
        }
    }

    /// A log that already carries a timestamp must be left alone, and one
    /// without a block number cannot be enriched — neither should panic.
    #[test]
    fn enrichment_targets_only_logs_that_need_it() {
        let mut logs = vec![log_at(100), log_at(100), log_at(101)];
        logs[0].block_timestamp = Some(1_700_000_000);

        let need: Vec<u64> = blocks_needing_timestamps(&logs);
        // Block 100 still needs it (logs[1]), 101 needs it, and the set is
        // deduplicated so one header fetch serves both logs at block 100.
        assert_eq!(need, vec![100, 101]);

        let mut none_needed = vec![log_at(7)];
        none_needed[0].block_timestamp = Some(42);
        assert!(blocks_needing_timestamps(&none_needed).is_empty());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --features collector --lib collector::utils`
Expected: FAIL — `blocks_needing_timestamps` not found.

- [ ] **Step 3: Write the implementation**

Add to `src/collector/utils.rs`:

```rust
use std::collections::{BTreeSet, HashMap};

/// Distinct block numbers among logs that still lack a timestamp.
///
/// Deduplicated so one header fetch serves every log in that block.
fn blocks_needing_timestamps(logs: &[Log]) -> Vec<u64> {
    logs.iter()
        .filter(|l| l.block_timestamp.is_none())
        .filter_map(|l| l.block_number)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Populate `block_timestamp` on logs whose RPC response omitted it.
///
/// `blockTimestamp` is not part of a standard `eth_getLogs` response and many
/// endpoints — Avalanche's among them — never send it, so logs arrive with
/// `block_timestamp: None`. LB pools need chain time to reproduce the
/// contract's volatility decay, and wall clock is not a substitute: it makes
/// event-replayed state diverge from freshly-fetched state permanently.
///
/// Fetches one header per distinct block, concurrently. Logs whose block
/// header cannot be read are left with `None`, so callers keep whatever
/// fallback they already have rather than getting a wrong timestamp.
pub async fn enrich_log_timestamps<P: Provider + Send + Sync>(
    provider: &Arc<P>,
    logs: &mut [Log],
) -> Result<()> {
    let wanted = blocks_needing_timestamps(logs);
    if wanted.is_empty() {
        return Ok(());
    }

    let futures = wanted.iter().map(|&n| {
        let provider = provider.clone();
        async move {
            let block = provider
                .get_block_by_number(BlockNumberOrTag::Number(n))
                .await
                .ok()
                .flatten();
            (n, block.map(|b| b.header.timestamp))
        }
    });
    let fetched: HashMap<u64, u64> = futures_util::future::join_all(futures)
        .await
        .into_iter()
        .filter_map(|(n, ts)| ts.map(|t| (n, t)))
        .collect();

    for log in logs.iter_mut() {
        if log.block_timestamp.is_none() {
            if let Some(n) = log.block_number {
                log.block_timestamp = fetched.get(&n).copied();
            }
        }
    }
    Ok(())
}
```

Re-export from `src/collector/mod.rs` — `pub use utils::*;` already covers it,
so verify rather than adding a duplicate export.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --features collector --lib collector::utils`
Expected: PASS.

- [ ] **Step 5: Call it from every BlockSource**

In `src/collector/block_source.rs`, enrich the log vector immediately before
each `Ok(EventBatch { .. })` return that carries events — in
`PendingBlockSource::next_batch`, `LatestBlockSource::next_batch`, and
`WebsocketBlockSource::next_batch`.

**Guard the cost.** This adds one `eth_getBlockByNumber` per distinct block
with events, on the collector's hot path. Only LB pools need it, so skip the
work entirely when the registry holds none:

```rust
if !self
    .pool_registry
    .get_addresses_by_type(PoolType::TraderJoeLB)
    .is_empty()
{
    enrich_log_timestamps(&self.provider, &mut events).await?;
}
```

A deployment with no LB pools therefore pays nothing, and a deployment with
them pays a handful of extra calls per batch — the price of correct quotes.

- [ ] **Step 6: Verify builds and regression**

Run: `cargo check && cargo check --features collector && cargo check --features rpc`
Run: `cargo test --features collector --lib`
Expected: all pass.

- [ ] **Step 7: Commit**

```bash
git add src/collector/utils.rs src/collector/block_source.rs src/collector/mod.rs
git commit -m "fix(collector): stamp block timestamps onto logs

blockTimestamp is not part of a standard eth_getLogs response and Avalanche
omits it, so every log reached apply_log with block_timestamp: None and took
the wall-clock fallback. That left the previous commit inert in production and
made event-replayed state unable to converge with freshly-fetched state.

Fetches one header per distinct block, concurrently, and only when the registry
actually holds LB pools — no other pool type has time-dependent state."
```

---

### Task 6: `QuoteContext` and time-aware trait methods

**Files:**
- Modify: `src/pool/base.rs`, `src/lb/pool.rs`, `src/lib.rs`
- Test: inline `mod tests` in `src/lb/pool.rs` (extend)

**Interfaces:**
- Produces: `pub struct QuoteContext { pub timestamp: u64 }`; defaulted `PoolInterface::calculate_output_at` and `calculate_input_at`. `LBPool` overrides both.

Additive only. V2, V3, and ERC4626 inherit the defaults and are untouched, so `evm-dex-arbitrage` compiles without edits.

- [ ] **Step 1: Write the failing test**

Append to the `mod tests` in `src/lb/pool.rs`:

```rust
    use crate::pool::base::QuoteContext;

    /// A later timestamp decays the volatility accumulator, so quoting the
    /// same input at two different times must be able to differ.
    #[test]
    fn calculate_output_at_honours_the_supplied_timestamp() {
        let mut pool = pool_with_time(1_700_000_000);
        pool.volatility_accumulator = 300_000;
        pool.volatility_reference = 300_000;

        let early = pool
            .calculate_output_at(
                &pool.token_x.clone(),
                U256::from(1_000u64),
                &QuoteContext { timestamp: 1_700_000_000 },
            )
            .unwrap();
        let late = pool
            .calculate_output_at(
                &pool.token_x.clone(),
                U256::from(1_000u64),
                &QuoteContext { timestamp: 1_700_100_000 },
            )
            .unwrap();

        assert!(early <= late, "decayed volatility should not raise the fee");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib lb::pool`
Expected: FAIL to compile — no method `calculate_output_at`.

- [ ] **Step 3: Add the trait surface**

In `src/pool/base.rs`, add above the `PoolInterface` trait:

```rust
/// Context for a quote whose result depends on when the swap executes.
///
/// Only pool types with time-dependent state consult this — currently only
/// Trader Joe LB, whose variable fee decays against `timeOfLastUpdate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuoteContext {
    /// Block timestamp the swap is expected to execute at, in seconds.
    pub timestamp: u64,
}
```

Add two defaulted methods to `PoolInterface`, after `calculate_input`:

```rust
    /// Time-aware variant of [`calculate_output`].
    ///
    /// Defaults to the timeless form, which is correct for every pool type
    /// with no time-dependent state (V2, V3, ERC4626).
    fn calculate_output_at(
        &self,
        token_in: &Address,
        amount_in: U256,
        _ctx: &QuoteContext,
    ) -> Result<U256> {
        self.calculate_output(token_in, amount_in)
    }

    /// Time-aware variant of [`calculate_input`].
    fn calculate_input_at(
        &self,
        token_out: &Address,
        amount_out: U256,
        _ctx: &QuoteContext,
    ) -> Result<U256> {
        self.calculate_input(token_out, amount_out)
    }
```

Export it from `src/pool/mod.rs` by extending the existing `pub use base::{...}` list with `QuoteContext`, and from `src/lib.rs` by extending the `pub use pool::{...}` list with `QuoteContext`.

- [ ] **Step 4: Override both on `LBPool`**

In the `impl PoolInterface for LBPool` block in `src/lb/pool.rs`, add after `calculate_input`. The bodies mirror the existing methods exactly, except they call the `_at` simulators with the caller's timestamp instead of `chrono::Utc::now()`:

```rust
    fn calculate_output_at(
        &self,
        token_in: &Address,
        amount_in: U256,
        ctx: &QuoteContext,
    ) -> Result<U256> {
        let swap_for_y = if token_in == &self.token_x {
            true
        } else if token_in == &self.token_y {
            false
        } else {
            return Err(anyhow!("Token {} not in LB pool {}", token_in, self.address));
        };

        let amount_in_128: u128 = amount_in
            .try_into()
            .map_err(|_| anyhow!("Amount too large for LB pool (exceeds u128)"))?;

        let (amount_in_left, amount_out, _fee) =
            self.simulate_swap_out_at(amount_in_128, swap_for_y, ctx.timestamp)?;
        if amount_in_left > 0 {
            return Err(anyhow!(
                "Insufficient liquidity in LB pool: {} of {} input remaining",
                amount_in_left,
                amount_in_128
            ));
        }
        Ok(U256::from(amount_out))
    }

    fn calculate_input_at(
        &self,
        token_out: &Address,
        amount_out: U256,
        ctx: &QuoteContext,
    ) -> Result<U256> {
        let swap_for_y = if token_out == &self.token_y {
            true
        } else if token_out == &self.token_x {
            false
        } else {
            return Err(anyhow!("Token {} not in LB pool {}", token_out, self.address));
        };

        let amount_out_128: u128 = amount_out
            .try_into()
            .map_err(|_| anyhow!("Amount too large for LB pool (exceeds u128)"))?;

        let (amount_in, amount_out_left, _fee) =
            self.simulate_swap_in_at(amount_out_128, swap_for_y, ctx.timestamp)?;
        if amount_out_left > 0 {
            return Err(anyhow!(
                "Insufficient liquidity in LB pool: {} of {} output remaining",
                amount_out_left,
                amount_out_128
            ));
        }
        Ok(U256::from(amount_in))
    }
```

Add `QuoteContext` to the `use crate::pool::base::{...}` import list at the top of `src/lb/pool.rs`.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib lb::pool`
Expected: PASS — 3 tests.

- [ ] **Step 6: Verify both builds**

Run: `cargo check && cargo check --features collector`
Expected: both succeed.

- [ ] **Step 7: Commit**

```bash
git add src/pool/base.rs src/pool/mod.rs src/lib.rs src/lb/pool.rs
git commit -m "feat: add QuoteContext and time-aware quote methods

calculate_output_at/calculate_input_at default to the timeless forms, so
V2/V3/ERC4626 and existing consumers are unaffected. LBPool overrides
them to thread an explicit timestamp into the volatility decay."
```

---

### Task 7: Event-replay convergence test (v2.1/v2.2)

**Files:**
- Create: `tests/lb_convergence.rs`
- Test: itself

**Interfaces:**
- Consumes: `fetch_lb_pool` (Task 4), block-timestamp handling (Task 5).
- Produces: `pub fn assert_lb_pools_converge(replayed: &LBPool, fetched: &LBPool, label: &str)` and `async fn converge_one(pool_address: Address, label: &str) -> Result<()>` — both reused by Task 11.

This is the primary correctness gate. It runs on v2.1/v2.2 only, to establish a green baseline on the versions that already work before v2.0 introduces new variables.

- [ ] **Step 1: Write the test**

Create `tests/lb_convergence.rs`:

```rust
//! Event-replay convergence: state built by applying logs from block A to
//! block B must equal state fetched fresh at block B.
//!
//! ```bash
//! cargo test --features collector test_lb_convergence -- --ignored --nocapture
//! ```

#![cfg(feature = "collector")]

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::primitives::{address, Address};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::Filter;
use anyhow::Result;

use evm_dex_pool::collector::enrich_log_timestamps;
use evm_dex_pool::lb::{fetch_lb_pool, LBPool};
use evm_dex_pool::{EventApplicable, TokenInfo, TopicList};

const RPC_URL: &str = "https://api.avax.network/ext/bc/C/rpc";
const CHAIN_ID: u64 = 43114;
const MULTICALL: Address = address!("cA11bde05977b3631167028862bE2a173976CA11");

/// How many blocks to replay. Wide enough to capture real swap activity.
const REPLAY_BLOCKS: u64 = 500;

const V22_POOL: Address = address!("8573f98175d816d520248b5facf40d309b1c9cee");
const V21_POOL: Address = address!("4224f6f4c9280509724db2dbac314621e4465c29");

struct CachingTokenInfo {
    cache: Arc<Mutex<HashMap<Address, (Address, u8)>>>,
}

impl CachingTokenInfo {
    fn new() -> Self {
        Self { cache: Arc::new(Mutex::new(HashMap::new())) }
    }
}

impl TokenInfo for CachingTokenInfo {
    fn get_or_fetch_token<P: Provider + Send + Sync>(
        &self,
        _provider: &Arc<P>,
        address: Address,
        _multicall_address: Address,
    ) -> impl Future<Output = Result<(Address, u8)>> + Send {
        let cache = Arc::clone(&self.cache);
        async move {
            let mut guard = cache.lock().unwrap();
            let entry = guard.entry(address).or_insert((address, 18u8));
            Ok(*entry)
        }
    }
}

/// Compare every field that mirrors on-chain state.
///
/// `last_updated` and `created_at` are excluded — they are local
/// bookkeeping, not chain state.
pub fn assert_lb_pools_converge(replayed: &LBPool, fetched: &LBPool, label: &str) {
    assert_eq!(replayed.version, fetched.version, "{label}: version");
    assert_eq!(replayed.bin_step, fetched.bin_step, "{label}: bin_step");
    assert_eq!(replayed.active_id, fetched.active_id, "{label}: active_id");

    assert_eq!(replayed.base_factor, fetched.base_factor, "{label}: base_factor");
    assert_eq!(replayed.filter_period, fetched.filter_period, "{label}: filter_period");
    assert_eq!(replayed.decay_period, fetched.decay_period, "{label}: decay_period");
    assert_eq!(replayed.reduction_factor, fetched.reduction_factor, "{label}: reduction_factor");
    assert_eq!(replayed.variable_fee_control, fetched.variable_fee_control, "{label}: variable_fee_control");
    assert_eq!(replayed.protocol_share, fetched.protocol_share, "{label}: protocol_share");
    assert_eq!(replayed.max_volatility_accumulator, fetched.max_volatility_accumulator, "{label}: max_volatility_accumulator");

    assert_eq!(replayed.volatility_accumulator, fetched.volatility_accumulator, "{label}: volatility_accumulator");
    assert_eq!(replayed.volatility_reference, fetched.volatility_reference, "{label}: volatility_reference");
    assert_eq!(replayed.id_reference, fetched.id_reference, "{label}: id_reference");
    assert_eq!(replayed.time_of_last_update, fetched.time_of_last_update, "{label}: time_of_last_update");

    // Bins are compared in full. Both sides already drop zero-reserve bins:
    // update_bin removes them, and the fetcher only inserts non-zero ones.
    assert_eq!(
        replayed.bins.len(),
        fetched.bins.len(),
        "{label}: bin count — replayed {} vs fetched {}",
        replayed.bins.len(),
        fetched.bins.len()
    );
    for (id, fetched_reserves) in &fetched.bins {
        let replayed_reserves = replayed
            .bins
            .get(id)
            .unwrap_or_else(|| panic!("{label}: bin {id} present after fetch but missing from replay"));
        assert_eq!(replayed_reserves, fetched_reserves, "{label}: bin {id} reserves");
    }
}

/// `pinned` selects the block range. `None` derives it from the chain head,
/// which suits busy pools. `Some((a, b))` uses a fixed historical range —
/// required for pools too quiet to guarantee logs near the head (see the
/// v2.0 fixture in Task 11). Archive `eth_call` is verified working at
/// 50,000 blocks back on this endpoint, so pinned ranges resolve.
async fn converge_one(
    pool_address: Address,
    label: &str,
    pinned: Option<(u64, u64)>,
) -> Result<()> {
    let provider = Arc::new(ProviderBuilder::new().connect_http(RPC_URL.parse()?));
    let token_info = CachingTokenInfo::new();

    let (block_a, block_b) = match pinned {
        Some(range) => range,
        None => {
            let head = provider.get_block_number().await?;
            (head - REPLAY_BLOCKS, head)
        }
    };

    println!("[{label}] replaying {pool_address} from {block_a} to {block_b}");

    // 1. Fetch at block A.
    let mut replayed = fetch_lb_pool(
        &provider,
        pool_address,
        BlockId::Number(BlockNumberOrTag::Number(block_a)),
        &token_info,
        MULTICALL,
        CHAIN_ID,
    )
    .await?;

    // 2. Replay every LB log in (A, B].
    let filter = Filter::new()
        .from_block(block_a + 1)
        .to_block(block_b)
        .address(pool_address)
        .event_signature(LBPool::topics());
    let mut logs = provider.get_logs(&filter).await?;
    println!("[{label}] applying {} logs", logs.len());
    assert!(!logs.is_empty(), "{label}: no logs in range — widen REPLAY_BLOCKS");

    // Required, not optional: `eth_getLogs` does not return `blockTimestamp`,
    // so every log arrives with `block_timestamp: None` and `apply_log` would
    // fall back to wall clock — guaranteeing `time_of_last_update` diverges
    // from the refetched value and failing this test for the wrong reason.
    // This is the same helper the collector's BlockSource uses (Task 5b).
    enrich_log_timestamps(&provider, &mut logs).await?;
    assert!(
        logs.iter().all(|l| l.block_timestamp.is_some()),
        "{label}: some logs still lack a block timestamp after enrichment"
    );

    for log in &logs {
        replayed.apply_log(log)?;
    }

    // 3. Fetch fresh at block B.
    let fetched = fetch_lb_pool(
        &provider,
        pool_address,
        BlockId::Number(BlockNumberOrTag::Number(block_b)),
        &token_info,
        MULTICALL,
        CHAIN_ID,
    )
    .await?;

    // 4. They must be identical.
    assert_lb_pools_converge(&replayed, &fetched, label);
    println!("[{label}] converged across {} bins", fetched.bins.len());
    Ok(())
}

#[tokio::test]
#[ignore]
async fn test_lb_convergence_v22() -> Result<()> {
    // Busy pool: ~20 logs per 2000 blocks. Head-derived range is fine.
    converge_one(V22_POOL, "v2.2", None).await
}

#[tokio::test]
#[ignore]
async fn test_lb_convergence_v21() -> Result<()> {
    // Busy pool: ~49 logs per 2000 blocks.
    converge_one(V21_POOL, "v2.1", None).await
}
```

**Note the `eth_getLogs` block-range cap.** This endpoint rejects ranges wider
than 2048 blocks (`requested too many blocks ... maximum is set to 2048`), and
`REPLAY_BLOCKS` is 500, so a single call is fine. If you ever widen the window
past 2048, the log fetch must be chunked.

- [ ] **Step 2: Run the test and record what fails**

Run: `cargo test --features collector test_lb_convergence -- --ignored --nocapture`

This is a discovery step. Expected outcomes, in the order they are likely to appear:

1. **Passes** — v2.1/v2.2 replay is already correct. Skip Task 8; note it in the commit.
2. **Fails on a bin's reserves** — most likely unhandled `CompositionFees`. Proceed to Task 8.
3. **Fails on `time_of_last_update`** — Task 5 is incomplete; a `chrono::Utc::now()` site was missed. Fix it there.
4. **Fails on `volatility_accumulator`** — the `Swap` handler takes it from the event, so a mismatch means the last log in range was not a `Swap`. Investigate before loosening anything.

Do **not** weaken any assertion to make this pass. Each failure is a real defect in shipped 1.4.0.

- [ ] **Step 3: Commit the test**

Commit the test whether or not it currently passes — a red test that encodes the correct invariant is worth having in history.

```bash
git add tests/lb_convergence.rs
git commit -m "test(lb): assert event replay converges with a fresh fetch

Fetches at block A, applies every LB log through block B, refetches at B,
and requires the two states to be identical field-for-field. This is the
gate that proves apply_log is correct."
```

---

### Task 7b: Reproduce `updateReferences` in `apply_log`

**Files:**
- Modify: `src/lb/pool.rs`
- Test: `tests/lb_convergence.rs` (rerun; no edits)

**Interfaces:**
- Consumes: `LBPool::update_references` (already exists, already correct), `Self::log_timestamp` (Task 5).
- Produces: no new API. `apply_log` becomes a faithful replay of the contract's swap-time reference update.

**This is a confirmed defect, found by Task 7's convergence test**, not a
hypothesis. On the v2.1 fixture, replay produced `volatility_reference` 10088
against a fetched 6250, and `id_reference` 8395262 against 8395261. All 1423
bins matched, so bin accounting is correct — the fee state is not.

Cause: `update_references` (`src/lb/pool.rs:167`) is a correct port of
`PairParameterHelper.updateReferences()`, but it is only ever called from
`simulate_swap_out_at` and `simulate_swap_in_at` — the read-only quote paths.
`apply_log` never calls it, so on replay it sets `id_reference` to the swap's
**final** bin (where the contract sets it to the **pre-swap** `activeId`) and
never writes `volatility_reference` at all.

The arithmetic reconciles exactly: `volAcc_pre 12500 → volRef 6250`
(reduction factor 5000) `→ 6250 + 10000 = 16250`, the fetched accumulator.

**Why it costs money.** Both fields feed quotes through `update_references`
whenever `dt < filter_period` — rapid successive swaps, which is exactly the
arbitrage window. Wrong volatility state there means wrong fees and wrong
quotes precisely when they matter most.

- [ ] **Step 1: Confirm the test fails on these fields**

Run: `cargo test --features collector test_lb_convergence_v21 -- --ignored --nocapture`
Expected: FAIL on `volatility_reference`, and behind it `id_reference`.
Record the actual numbers — they will differ from the run above, since the
range is head-derived.

- [ ] **Step 2: Call `update_references` in the Swap arm**

In `src/lb/pool.rs`, in `apply_log`'s `ILBPair::Swap` arm, mirror the contract's
own ordering. `LBPair.swap()` calls `updateReferences(block.timestamp)` **once,
before** the bin loop, then updates the accumulator per bin, then stamps
`timeOfLastUpdate`.

Order is load-bearing: `update_references` reads `self.active_id` to derive
`id_ref`, so it must run **before** `active_id` is overwritten with the event's
bin, and before `time_of_last_update` is restamped.

```rust
                // Mirror LBPair.swap(): updateReferences() runs once, before
                // the bin loop, using PRE-swap state. It reads self.active_id
                // for id_ref and self.time_of_last_update for dt, so it must
                // run before either is overwritten below.
                let ts = Self::log_timestamp(event);
                let (vol_ref, id_ref) = self.update_references(ts);
                self.volatility_reference = vol_ref;
                self.id_reference = id_ref;

                // Now apply the event's own state.
                self.active_id = id;
                self.volatility_accumulator = swap_data.volatilityAccumulator.to();
                self.time_of_last_update = ts;
```

Delete the existing `self.id_reference = id;` line — that assignment is the
bug. Keep `self.last_updated = chrono::Utc::now()…` on wall clock (Task 5).

**Multi-bin swaps are self-correcting, and you should confirm you understand
why before changing anything.** One `swap()` crossing N bins emits N `Swap`
logs, so replay calls `update_references` N times where the contract called it
once. That is harmless: after the first log sets `time_of_last_update = ts`,
every subsequent log in the same transaction computes `dt = 0`, which is
`< filter_period`, so the update branch does not fire and both references stay
put. The first log's values survive — which is the contract's behaviour.

- [ ] **Step 3: Rerun convergence**

Run: `cargo test --features collector test_lb_convergence -- --ignored --nocapture`
Expected: **both** pools converge on every compared field.

If v2.1 now converges but some other field diverges, report it — do not adjust
the assertion.

- [ ] **Step 4: Regression**

Run: `cargo test --features collector --lib`
Run: `cargo test --features collector test_lb_fuzz -- --ignored --nocapture`
Expected: lib suite green; fuzz still 260/260. The fuzz test exercises
`simulate_swap_*_at`, which already called `update_references` — this change
must not alter quote parity.

- [ ] **Step 5: Verify builds**

Run: `cargo check && cargo check --features collector && cargo check --features rpc`

- [ ] **Step 6: Commit**

```bash
git add src/lb/pool.rs
git commit -m "fix(lb): reproduce updateReferences when replaying swaps

apply_log never called update_references, so replayed state carried a
volatility_reference that was never written and an id_reference set to the
swap's final bin rather than the pre-swap activeId. Both feed quotes through
update_references whenever dt < filter_period — rapid successive swaps, the
arbitrage window — so the error surfaced exactly where quotes matter most.

Found by the event-replay convergence test: v2.1 replay produced volRef 10088
against a fetched 6250."
```

---

### Task 8: Pin every convergence range and assert event coverage

**Files:**
- Modify: `tests/common/mod.rs`, `tests/lb_convergence.rs`
- Test: itself, plus a new unit test in `src/lb/pool.rs`

**Interfaces:**
- Consumes: `converge_one`, `assert_lb_pools_converge` (Task 7).
- Produces: `converge_one` chunks its log fetch; `assert_range_covers` in `tests/common/mod.rs`.

**Composition fees need no handler — settled, do not implement one.** The
previous pinned range contained two `CompositionFees` events. `converge_one`
filters by `LBPool::topics()`, which excludes them, so they were never applied
— and bins still matched exactly across all 1423 under `u128` equality. Each
fires in the same transaction as a `DepositedToBins`, so the deposit event
already reports amounts inclusive of the fee. **Adding a handler would
double-count and turn a passing gate into a failing one.**

**Why this task exists.** A green convergence test proves nothing if its range
never contained the events it claims to exercise. That is not hypothetical
here — a head-derived range passed against known-broken code because it
happened to hold no swap pair far enough apart to trigger the bug, and a
v2.2 run "converged" on 9 logs that were all Swaps.

Archive access is confirmed: the endpoint serves `eth_call` 10,000,000 blocks
back, so any historical range is reachable. Live head-derived ranges are
replaced entirely by fixed ones.

- [ ] **Step 1: Chunk the log fetch**

Ranges below span up to 11,000 blocks and `eth_getLogs` rejects anything wider
than 2048 on this endpoint. In `tests/common/mod.rs`, replace the single
`provider.get_logs(&filter)` call in `converge_one` with a loop over
2000-block windows, concatenating in order:

```rust
    const CHUNK: u64 = 2000;
    let mut logs = Vec::new();
    let mut from = block_a + 1;
    while from <= block_b {
        let to = (from + CHUNK - 1).min(block_b);
        let filter = Filter::new()
            .from_block(from)
            .to_block(to)
            .address(pool_address)
            .event_signature(LBPool::topics());
        logs.extend(provider.get_logs(&filter).await?);
        from = to + 1;
    }
```

- [ ] **Step 2: Add the coverage assertion**

Also in `tests/common/mod.rs`. This is the point of the task — a range that
stops covering an event must fail loudly rather than pass quietly:

```rust
/// Assert the replayed range actually contains every event kind named.
///
/// Without this a quiet or drifted range yields a green test that exercised
/// nothing — which has already happened twice on this branch. Prints the
/// per-topic counts so a shrinking fixture is visible before it hits zero.
pub fn assert_range_covers(logs: &[Log], required: &[(&str, B256)], label: &str) {
    for (name, topic) in required {
        let n = logs.iter().filter(|l| l.topic0() == Some(topic)).count();
        println!("[{label}] coverage: {name} = {n}");
        assert!(
            n > 0,
            "{label}: pinned range contains no {name} event, so this test \
             would pass without ever exercising it. Re-pin the range."
        );
    }
}
```

Call it in `converge_one` immediately after the fetch and before replay.
The required set for v2.1/v2.2 is every topic `apply_log` handles and that
occurs on-chain: `ILBPair::Swap`, `ILBPair::DepositedToBins`,
`ILBPair::WithdrawnFromBins`. Take it as a parameter so Task 11 can pass
v2.0's set.

- [ ] **Step 3: Pin every range**

Replace the head-derived tests. All values below were verified live by
chunked `eth_getLogs` over the exact ranges given.

```rust
/// v2.1. Verified: 302 logs — 282 Swap, 4 DepositedToBins, 4
/// WithdrawnFromBins, 4 CompositionFees, 8 TransferBatch. Also contains swap
/// gaps both above and below the pool's 30s filterPeriod, so it exercises
/// both branches of update_references.
const V21_RANGE: (u64, u64) = (93_341_000, 93_348_000);

/// v2.2. Verified: 164 logs — 159 Swap, 1 DepositedToBins, 1
/// WithdrawnFromBins, 1 CompositionFees, 2 TransferBatch. Deliberately
/// 11,000 blocks wide: this pool's non-Swap events are sparse enough that a
/// narrow window catches only Swaps, which is how an earlier run "converged"
/// on 9 logs while exercising one code path.
const V22_RANGE: (u64, u64) = (93_271_000, 93_282_000);
```

Point `test_lb_convergence_v21` and `test_lb_convergence_v22` at these.
**Delete `test_lb_convergence_v21_head`** — head-derived ranges are being
retired, and it is the specific construct that passed against broken code.

- [ ] **Step 4: Cover `StaticFeeParametersSet` with a unit test**

`apply_log` handles this event, but it **never occurs** — a scan of 120,000
blocks across all three fixture pools found zero. No pinned range can cover
it, so the coverage assertion must not require it, and it needs a unit test
instead. Add to `mod tests` in `src/lb/pool.rs`, following the existing
`swap_log` helper's construction:

```rust
    /// StaticFeeParametersSet never fires on any live fixture pool — 120,000
    /// blocks scanned, zero occurrences — so no convergence range can cover
    /// this arm. Unit-tested instead, or it would ship unexercised.
    #[test]
    fn apply_log_updates_static_fee_parameters() {
        let mut pool = pool_with_time(1_700_000_000);
        let event = ILBPair::StaticFeeParametersSet {
            sender: Address::ZERO,
            baseFactor: 7777,
            filterPeriod: 44,
            decayPeriod: 888,
            reductionFactor: 4444,
            variableFeeControl: 55555u32.try_into().unwrap(),
            protocolShare: 1234,
            maxVolatilityAccumulator: 222222u32.try_into().unwrap(),
        };
        let log = Log {
            inner: alloy::primitives::Log {
                address: Address::ZERO,
                data: event.encode_log_data(),
            },
            block_timestamp: Some(1_700_000_500),
            ..Default::default()
        };
        pool.apply_log(&log).unwrap();

        assert_eq!(pool.base_factor, 7777);
        assert_eq!(pool.filter_period, 44);
        assert_eq!(pool.decay_period, 888);
        assert_eq!(pool.reduction_factor, 4444);
        assert_eq!(pool.variable_fee_control, 55555);
        assert_eq!(pool.protocol_share, 1234);
        assert_eq!(pool.max_volatility_accumulator, 222222);
    }
```

Check the field names against `contracts/ABI/ILBPair.json` before writing —
adapt to what `sol!` actually generates, keeping every field asserted.

- [ ] **Step 5: Run and verify coverage output**

Run: `cargo test --features collector test_lb_convergence -- --ignored --nocapture`

Both tests must pass **and** print non-zero counts for all three required
topics. Paste that coverage output in your report — it is the deliverable.

Run: `cargo test --features collector --lib` — the new unit test included.

- [ ] **Step 6: Verify builds**

Run: `cargo check && cargo check --features collector && cargo check --features rpc`

- [ ] **Step 7: Commit**

```bash
git add tests/common/mod.rs tests/lb_convergence.rs src/lb/pool.rs
git commit -m "test(lb): pin every convergence range and assert event coverage

Head-derived ranges made the gate unreliable in both directions: one passed
against known-broken code, and a v2.2 run converged on 9 logs that were all
Swaps, exercising a single path. Both ranges are now fixed and verified to
contain every event apply_log handles, the log fetch chunks to stay under the
2048-block eth_getLogs cap, and the test asserts coverage rather than assuming
it — a range that stops covering an event now fails loudly.

StaticFeeParametersSet never occurs on any fixture pool (zero in 120,000
blocks), so it is unit-tested rather than left unexercised."
```

---

### Task 9: v2.0 fetch path

**Files:**
- Modify: `src/lb/fetcher.rs`
- Test: `tests/lb_version_detect.rs` (extend)

**Interfaces:**
- Consumes: `RpcILBPairV20` (Task 2), `tree_base_slot` returning `None` for v2.0 (Task 4).
- Produces: `fetch_lb_pool` returns a correctly populated `LBPool` for v2.0 pairs.

v2.0 exposes `getReservesAndId()` and a single 12-field `feeParameters()` struct instead of v2.1+'s split getters, and it has no `getBinStep()` — bin step lives inside `feeParameters`.

- [ ] **Step 1: Write the failing test**

Append to `tests/lb_version_detect.rs`:

```rust
#[tokio::test]
#[ignore]
async fn test_fetch_v20_pool() -> Result<()> {
    let provider = Arc::new(ProviderBuilder::new().connect_http(RPC_URL.parse()?));
    let pool = fetch_lb_pool(
        &provider, V20_POOL, BlockId::latest(), &NoopTokenInfo, MULTICALL, 43114,
    )
    .await?;

    assert_eq!(pool.version, LBVersion::V2_0);
    assert!(pool.bin_step > 0, "bin_step must come from feeParameters");
    assert!(pool.active_id > 0, "active_id must come from getReservesAndId");
    assert!(pool.bins.len() > 1, "expected multiple non-empty bins, got {}", pool.bins.len());
    assert!(pool.hooks_parameters.is_none(), "v2.0 has no hooks");
    Ok(())
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --features collector test_fetch_v20 -- --ignored --nocapture`
Expected: FAIL — the v2.1+ `getTokenX()` multicall reverts on a v2.0 pair.

- [ ] **Step 3: Split the state fetch by version**

In `src/lb/fetcher.rs`, add `use crate::contracts_rpc::RpcILBPairV20;` to the imports.

Introduce a small struct above `fetch_lb_pool` to carry the version-independent result:

```rust
/// Pool state normalised across LB versions.
struct LBState {
    token_x_raw: Address,
    token_y_raw: Address,
    bin_step: u16,
    active_id: u32,
    base_factor: u16,
    filter_period: u16,
    decay_period: u16,
    reduction_factor: u16,
    variable_fee_control: u32,
    protocol_share: u16,
    max_volatility_accumulator: u32,
    volatility_accumulator: u32,
    volatility_reference: u32,
    id_reference: u32,
    time_of_last_update: u64,
}
```

Add the v2.0 reader:

```rust
/// Read v2.0 state. v2.0 packs every fee parameter into one `feeParameters()`
/// struct and has no `getBinStep()`; bin step is field 0 of that struct.
async fn fetch_v20_state<P: Provider + Send + Sync>(
    provider: &Arc<P>,
    pool_address: Address,
    block_number: BlockId,
    multicall_address: Address,
) -> Result<LBState> {
    let lb = RpcILBPairV20::new(pool_address, provider);
    let r = provider
        .multicall()
        .address(multicall_address)
        .add(lb.tokenX()) // 0
        .add(lb.tokenY()) // 1
        .add(lb.getReservesAndId()) // 2
        .add(lb.feeParameters()) // 3
        .block(block_number)
        .try_aggregate(false)
        .await?;

    let token_x_raw = r.0?;
    let token_y_raw = r.1?;
    let reserves = r.2?;
    let fp = r.3?;

    Ok(LBState {
        token_x_raw,
        token_y_raw,
        bin_step: fp.binStep,
        active_id: reserves.activeId.to(),
        base_factor: fp.baseFactor,
        filter_period: fp.filterPeriod,
        decay_period: fp.decayPeriod,
        reduction_factor: fp.reductionFactor,
        variable_fee_control: fp.variableFeeControl.to(),
        protocol_share: fp.protocolShare,
        max_volatility_accumulator: fp.maxVolatilityAccumulated.to(),
        volatility_accumulator: fp.volatilityAccumulated.to(),
        volatility_reference: fp.volatilityReference.to(),
        id_reference: fp.indexRef.to(),
        time_of_last_update: fp.time.to(),
    })
}
```

Wrap the existing Batch 1 multicall into a matching v2.1+ reader:

```rust
/// Read v2.1/v2.2 state. Both expose the same split getters.
async fn fetch_v21_state<P: Provider + Send + Sync>(
    provider: &Arc<P>,
    pool_address: Address,
    block_number: BlockId,
    multicall_address: Address,
) -> Result<LBState> {
    let lb = ILBPair::new(pool_address, provider);
    let r = provider
        .multicall()
        .address(multicall_address)
        .add(lb.getTokenX()) // 0
        .add(lb.getTokenY()) // 1
        .add(lb.getBinStep()) // 2
        .add(lb.getActiveId()) // 3
        .add(lb.getStaticFeeParameters()) // 4
        .add(lb.getVariableFeeParameters()) // 5
        .block(block_number)
        .try_aggregate(false)
        .await?;

    let token_x_raw = r.0?;
    let token_y_raw = r.1?;
    let bin_step = r.2?;
    let active_id: u32 = r.3?.to();
    let s = r.4?;
    let v = r.5?;

    Ok(LBState {
        token_x_raw,
        token_y_raw,
        bin_step,
        active_id,
        base_factor: s.baseFactor,
        filter_period: s.filterPeriod,
        decay_period: s.decayPeriod,
        reduction_factor: s.reductionFactor,
        variable_fee_control: s.variableFeeControl.to(),
        protocol_share: s.protocolShare,
        max_volatility_accumulator: s.maxVolatilityAccumulator.to(),
        volatility_accumulator: v.volatilityAccumulator.to(),
        volatility_reference: v.volatilityReference.to(),
        id_reference: v.idReference.to(),
        time_of_last_update: v.timeOfLastUpdate.to(),
    })
}
```

Then dispatch in `fetch_lb_pool`:

```rust
    let state = match version {
        LBVersion::V2_0 => {
            fetch_v20_state(provider, pool_address, block_number, multicall_address).await?
        }
        LBVersion::V2_1 | LBVersion::V2_2 => {
            fetch_v21_state(provider, pool_address, block_number, multicall_address).await?
        }
    };
```

Replace the individual `static_fees.*` / `var_fees.*` arguments in the `LBPool::new(..)` call with the corresponding `state.*` fields, and use `state.active_id` for the bin walk.

- [ ] **Step 4: Route v2.0 bin reserve reads to the v2.0 `getBin`**

v2.0's `getBin` returns `uint256` reserves rather than v2.1+'s `uint128`, and `MulticallBuilder::new_dynamic` requires one call type per builder, so the two paths need separate builders. Replace the whole Batch 3 loop with:

```rust
    // ── Batch 3: Fetch bin reserves via multicall ────────────────────────
    let mut bins = BTreeMap::new();
    let lb20_instance = RpcILBPairV20::new(pool_address, provider);

    for chunk in bin_ids.chunks(250) {
        match version {
            LBVersion::V2_0 => {
                let mut multicall =
                    MulticallBuilder::new_dynamic(provider).address(multicall_address);
                for &id in chunk {
                    multicall = multicall.add_dynamic(lb20_instance.getBin(U24::from(id)));
                }
                let results = multicall.block(block_number).aggregate().await?;
                for (i, &id) in chunk.iter().enumerate() {
                    // v2.0 returns uint256; real bin reserves always fit u128.
                    let rx: u128 = results[i].reserveX.try_into().unwrap_or(0);
                    let ry: u128 = results[i].reserveY.try_into().unwrap_or(0);
                    if rx > 0 || ry > 0 {
                        bins.insert(id, (rx, ry));
                    }
                }
            }
            LBVersion::V2_1 | LBVersion::V2_2 => {
                let mut multicall =
                    MulticallBuilder::new_dynamic(provider).address(multicall_address);
                for &id in chunk {
                    multicall = multicall.add_dynamic(lb_instance.getBin(U24::from(id)));
                }
                let results = multicall.block(block_number).aggregate().await?;
                for (i, &id) in chunk.iter().enumerate() {
                    let rx: u128 = results[i].binReserveX;
                    let ry: u128 = results[i].binReserveY;
                    if rx > 0 || ry > 0 {
                        bins.insert(id, (rx, ry));
                    }
                }
            }
        }
    }
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test --features collector test_fetch_v20 -- --ignored --nocapture`
Expected: PASS.

- [ ] **Step 6: Verify no regression**

Run: `cargo test --features collector test_fetch_stamps test_lb_fuzz -- --ignored --nocapture`
Expected: PASS — v2.1/v2.2 fetch is unchanged.

- [ ] **Step 7: Verify both builds**

Run: `cargo check && cargo check --features collector`
Expected: both succeed.

- [ ] **Step 8: Commit**

```bash
git add src/lb/fetcher.rs tests/lb_version_detect.rs
git commit -m "feat(lb): fetch v2.0 pool state

v2.0 exposes getReservesAndId() and a single 12-field feeParameters()
struct instead of v2.1+'s split getters, and carries bin step inside that
struct rather than in getBinStep(). Both paths normalise onto LBState."
```

---

### Task 10: v2.0 event decoding

**Files:**
- Modify: `src/lb/pool.rs`
- Test: inline `mod tests` in `src/lb/pool.rs` (extend), then `tests/lb_convergence.rs`

**Interfaces:**
- Consumes: `ILBPairV20` events (Task 2).
- Produces: `LBPool::topics()` includes the v2.0 topic set; `apply_log` handles v2.0 `Swap`, `DepositedToBin`, `WithdrawnFromBin`, `CompositionFee`.

**This task carries the design's one genuine unknown.** v2.0's `Swap` emits `amountIn` as `_amountInToBin`, excluding all fees, whereas v2.1+ emits fee-inclusive `amountsIn`. v2.0 also keeps LP fees claimable rather than compounding them, and `getReservesAndId()` reports reserves excluding all fees. Whether `bin += amountIn - amountOut` is therefore correct for v2.0 depends on whether v2.0's `getBin()` likewise excludes fees. **Do not assume.** Implement the straightforward form first, then let the Task 11 convergence run decide.

- [ ] **Step 1: Register v2.0 topics**

In `impl TopicList for LBPool`, extend `topics()`:

```rust
            ILBPairV20::Swap::SIGNATURE_HASH,
            ILBPairV20::DepositedToBin::SIGNATURE_HASH,
            ILBPairV20::WithdrawnFromBin::SIGNATURE_HASH,
            ILBPairV20::CompositionFee::SIGNATURE_HASH,
```

and `profitable_topics()`:

```rust
            ILBPairV20::Swap::SIGNATURE_HASH,
```

Add `use crate::contracts::ILBPairV20;` to the imports.

Because `topics()` is a static method, every LB pool now registers both generations' topics. This widens the global log filter slightly but is not a correctness issue: the filter is also address-scoped, and a given pair only ever emits its own generation's events.

- [ ] **Step 2: Handle the v2.0 events**

Add these arms to `apply_log`, before the catch-all. Note v2.0 carries separate `amountX`/`amountY` fields rather than packed `bytes32`, so `decode_amounts` is not used:

```rust
            Some(&ILBPairV20::Swap::SIGNATURE_HASH) => {
                let d: ILBPairV20::Swap = event.log_decode()?.inner.data;
                let id: u32 = d.id.to();
                // Fail loudly rather than saturating: an amount that does not
                // fit u128 is malformed, and clamping to u128::MAX would
                // corrupt the bin silently.
                let amount_in: u128 = d.amountIn.try_into()
                    .map_err(|_| anyhow!("v2.0 Swap amountIn exceeds u128"))?;
                let amount_out: u128 = d.amountOut.try_into()
                    .map_err(|_| anyhow!("v2.0 Swap amountOut exceeds u128"))?;

                let (rx, ry) = self.bins.get(&id).copied().unwrap_or((0, 0));
                // swapForY: X goes in, Y comes out.
                let (new_rx, new_ry) = if d.swapForY {
                    (rx.saturating_add(amount_in), ry.saturating_sub(amount_out))
                } else {
                    (rx.saturating_sub(amount_out), ry.saturating_add(amount_in))
                };
                self.update_bin(id, new_rx, new_ry);

                // Same updateReferences ordering as the v2.1+ arm — see the
                // comment there. This MUST run on PRE-swap state, so before
                // active_id / time_of_last_update are overwritten below.
                // Writing `id_reference = id` here instead was the exact bug
                // Task 7b found on v2.1: it sets the reference to the swap's
                // FINAL bin rather than the pre-swap activeId, and never
                // writes volatility_reference at all.
                let ts = Self::log_timestamp(event);
                let (vol_ref, id_ref) = self.update_references(ts);
                self.volatility_reference = vol_ref;
                self.id_reference = id_ref;

                self.active_id = id;
                self.volatility_accumulator = d.volatilityAccumulated.to();
                self.time_of_last_update = ts;
                // last_updated is local bookkeeping and stays wall clock.
                self.last_updated = chrono::Utc::now().timestamp() as u64;
                Ok(())
            }
            Some(&ILBPairV20::DepositedToBin::SIGNATURE_HASH) => {
                let d: ILBPairV20::DepositedToBin = event.log_decode()?.inner.data;
                let id: u32 = d.id.to();
                let (rx, ry) = self.bins.get(&id).copied().unwrap_or((0, 0));
                self.update_bin(
                    id,
                    rx.saturating_add(d.amountX.try_into().unwrap_or(0)),
                    ry.saturating_add(d.amountY.try_into().unwrap_or(0)),
                );
                self.last_updated = Self::log_timestamp(event);
                Ok(())
            }
            Some(&ILBPairV20::WithdrawnFromBin::SIGNATURE_HASH) => {
                let d: ILBPairV20::WithdrawnFromBin = event.log_decode()?.inner.data;
                let id: u32 = d.id.to();
                let (rx, ry) = self.bins.get(&id).copied().unwrap_or((0, 0));
                self.update_bin(
                    id,
                    rx.saturating_sub(d.amountX.try_into().unwrap_or(0)),
                    ry.saturating_sub(d.amountY.try_into().unwrap_or(0)),
                );
                self.last_updated = Self::log_timestamp(event);
                Ok(())
            }
            Some(&ILBPairV20::CompositionFee::SIGNATURE_HASH) => {
                let d: ILBPairV20::CompositionFee = event.log_decode()?.inner.data;
                let id: u32 = d.id.to();
                let (rx, ry) = self.bins.get(&id).copied().unwrap_or((0, 0));
                self.update_bin(
                    id,
                    rx.saturating_add(d.feesX.try_into().unwrap_or(0)),
                    ry.saturating_add(d.feesY.try_into().unwrap_or(0)),
                );
                self.last_updated = Self::log_timestamp(event);
                Ok(())
            }
```

- [ ] **Step 3: Write a unit test proving the generations do not collide**

Append to the `mod tests` in `src/lb/pool.rs`:

```rust
    #[test]
    fn topics_cover_both_generations_without_collision() {
        let topics = LBPool::topics();
        assert!(topics.contains(&ILBPair::Swap::SIGNATURE_HASH));
        assert!(topics.contains(&crate::contracts::ILBPairV20::Swap::SIGNATURE_HASH));

        let mut sorted = topics.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), topics.len(), "duplicate topic registered");
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib lb::pool`
Expected: PASS.

- [ ] **Step 5: Verify both builds**

Run: `cargo check && cargo check --features collector`
Expected: both succeed.

- [ ] **Step 6: Commit**

```bash
git add src/lb/pool.rs
git commit -m "feat(lb): decode v2.0 events

v2.0 Swap/DepositedToBin/WithdrawnFromBin/CompositionFee carry separate
amountX/amountY fields rather than packed bytes32, and have distinct
topic0s, so apply_log's existing topic dispatch separates the two
generations without consulting the version field.

Bin accounting uses the straightforward amountIn - amountOut form; the
v2.0 convergence test decides whether the differing fee semantics require
folding the fees field in."
```

---

### Task 11: Extend both acceptance tests to v2.0

**Files:**
- Modify: `tests/lb_convergence.rs`, `tests/lb_pool.rs`

**Interfaces:**
- Consumes: everything above.

Runs last and alone, so that when the §6.3 v2.0 accounting question surfaces it is the only variable in play.

- [ ] **Step 1: Add v2.0 convergence with an honest required set**

The v2.0 fixture pool is severely dormant. Measured across 500,000 blocks:
199 Swap, 16 `WithdrawnFromBin`, 18 `FeesCollected`, 16 `TransferSingle` —
and **zero `DepositedToBin`, zero `CompositionFee`**. Nobody has added
liquidity to this deprecated pool in the reachable window; they are only
exiting. Exactly one 2,000-block window in 300,000 contains both a Swap and a
`WithdrawnFromBin`.

So the v2.0 required-topic set is `Swap` and `WithdrawnFromBin` only. The two
uncovered arms get unit tests in Step 2 — the same treatment
`StaticFeeParametersSet` received, and for the same reason: an arm no
reachable range can exercise must be tested some other way, not quietly
assumed.

```rust
/// v2.0. Verified: 2 Swap (blocks 93299096 and 93299113, 17s apart — above
/// the pool's 10s filterPeriod, so update_references fires) and 5
/// WithdrawnFromBin, plus FeesCollected/TransferSingle/TransferBatch as
/// bystanders.
///
/// This is the ONLY window in 300,000 blocks containing both a Swap and a
/// WithdrawnFromBin. DepositedToBin and CompositionFee do not occur at all
/// in 500,000 blocks — see the unit tests in src/lb/pool.rs.
///
/// Expect this test to take ~40s: v2.0 has no corroborated bin-tree layout,
/// so each of the two fetches walks bins sequentially (~19s for 68 bins).
const V20_RANGE: (u64, u64) = (93_298_348, 93_300_348);

#[tokio::test]
#[ignore]
async fn test_lb_convergence_v20() -> Result<()> {
    converge_one(V20_POOL, "v2.0", Some(V20_RANGE), &v20_required_topics()).await
}
```

Build `v20_required_topics()` from the `ILBPairV20` bindings the same way the
v2.1+ set is built — derive the hashes, never hardcode hex. For reference,
these were confirmed by printing the bindings' own constants:

```
Swap              0xc528cda9e500228b16ce84fadae290d9a49aecb17483110004c5af0a07f6fd73
WithdrawnFromBin  0xda5e7177dface55f5e0eff7dfc67420a1db4243ddfcf0ecc84ed93e034dd8cc2
DepositedToBin    0x4216cc3b…
CompositionFee    0x56f8e764…
```

**This is the step that adjudicates the v2.0 fee-accounting question.** If bins
diverge, the `Swap` arm's `bin += amountIn - amountOut` is wrong for v2.0 and
the `fees` field needs folding in. Report the direction of the discrepancy —
replayed lower than fetched means fees are missing from the input side. Do not
adjust the assertion.

- [ ] **Step 2: Unit-test the two arms no range can cover**

Add to `mod tests` in `src/lb/pool.rs`, alongside the existing
`StaticFeeParametersSet` test. Follow that test's construction pattern.

Cover `ILBPairV20::DepositedToBin` and `ILBPairV20::CompositionFee`: build a
pool with a known bin, apply the log, and assert the bin's reserves moved by
exactly the event's amounts. Assert both `reserve_x` and `reserve_y`, and use
a bin id that already has non-zero reserves so an accidental no-op cannot pass.

Note in a comment that these are unit-tested because the events do not occur
on any reachable v2.0 fixture — zero in 500,000 blocks — so a future reader
does not assume the convergence test covers them.

- [ ] **Step 3: Add v2.0 quote parity**

v2.0 pairs have **no `getSwapOut`/`getSwapIn` on the pair** — those live on the v2.0 router. In `tests/lb_pool.rs`, add a v2.0 fixture list and a router-based reference path:

```rust
/// LB v2.0 router on Avalanche. v2.0 pairs expose no on-pair quoter, so the
/// on-chain reference for exact-in comes from the router.
const V20_ROUTER: &str = "0xE3Ffc583dC176575eEA7FD9dF2A7c65F7E23f4C3";

const V20_TEST_POOLS: &[&str] = &[
    "0x18332988456C4Bd9ABa6698ec748b331516F5A14", // WAVAX/USDC
];

sol! {
    #[sol(rpc)]
    interface ILBRouterV20 {
        function getSwapOut(address lbPair, uint256 amountIn, bool swapForY)
            external view returns (uint256 amountOut, uint256 feesIn);
    }
}
```

Then add the test itself:

```rust
#[tokio::test]
#[ignore]
async fn test_lb_v20_fuzz() -> Result<()> {
    let provider = Arc::new(ProviderBuilder::new().connect_http(RPC_URL.parse()?));
    let token_info = SimpleTokenCache::new();

    // Pin a block so the on-chain reference and the offline simulation see
    // identical state, and read its timestamp for the volatility decay.
    let block_num = provider.get_block_number().await?;
    let block = BlockId::Number(BlockNumberOrTag::Number(block_num));
    let block_ts = provider
        .get_block_by_number(BlockNumberOrTag::Number(block_num))
        .await?
        .expect("block exists")
        .header
        .timestamp;

    let router = ILBRouterV20::new(V20_ROUTER.parse::<Address>()?, &provider);

    for pool_str in V20_TEST_POOLS {
        let pool_addr: Address = pool_str.parse()?;
        let pool = fetch_lb_pool(
            &provider, pool_addr, block, &token_info, MULTICALL.parse()?, CHAIN_ID,
        )
        .await?;

        for &mult in FUZZ_MULTIPLIERS {
            for swap_for_y in [true, false] {
                let amount_in = U256::from(mult) * U256::from(10u128.pow(14));

                let onchain = router
                    .getSwapOut(pool_addr, amount_in, swap_for_y)
                    .block(block)
                    .call()
                    .await?;

                let (amount_in_left, offchain_out, _fee) = pool.simulate_swap_out_at(
                    amount_in.try_into().unwrap(),
                    swap_for_y,
                    block_ts,
                )?;

                // The router reverts or returns 0 when liquidity runs out;
                // our simulator reports the shortfall instead. Skip those.
                if amount_in_left > 0 {
                    continue;
                }
                assert_eq!(
                    U256::from(offchain_out),
                    onchain.amountOut,
                    "v2.0 {pool_addr} mult={mult} swap_for_y={swap_for_y}: \
                     offchain {offchain_out} vs onchain {}",
                    onchain.amountOut
                );
            }
        }
        println!("v2.0 {pool_addr}: quote parity across {} bins", pool.bins.len());
    }
    Ok(())
}
```

If the v2.0 router address above is wrong for the target chain, read it from the pair's own `factory()` and look up the router from LFJ's deployment list rather than guessing.

- [ ] **Step 4: Run the full LB suite**

Run: `cargo test --features collector test_lb test_detect test_fetch -- --ignored --nocapture`
Expected: PASS across v2.0, v2.1, and v2.2.

- [ ] **Step 5: Verify both builds and lint**

Run: `cargo check && cargo check --features collector && cargo fmt && cargo clippy --features collector`
Expected: clean.

- [ ] **Step 6: Bump the crate version**

In `Cargo.toml`, bump `version` from `1.4.0` to `1.5.0` — additive API, no breaking change for 1.4.0 consumers.

- [ ] **Step 7: Commit**

```bash
git add tests/lb_convergence.rs tests/lb_pool.rs Cargo.toml
git commit -m "test(lb): extend quote parity and convergence to v2.0

Quote parity for v2.0 sources its on-chain reference from the v2.0 router,
since v2.0 pairs expose no on-pair getSwapOut. Bumps to 1.5.0 — the API is
additive and 1.4.0 consumers are unaffected."
```

---

## Verification Summary

| Acceptance criterion | Test | Task |
|---|---|---|
| Offchain quote == onchain quote, same block | `test_lb_fuzz` (v2.1/v2.2), extended for v2.0 | 11 |
| Fetch at A → replay A..B → refetch at B → identical | `test_lb_convergence_{v20,v21,v22}` | 7, 11 |
| v2.0 pairs classified correctly | `test_v20_pair_is_not_misclassified` | 3 |
| 1.4.0 snapshots still deserialize | `pool_without_version_field_deserializes_as_v21` | 1 |
| ABI matches deployed contracts | `v20_selectors_match_deployed_contracts` | 2 |

## Coverage Gap — CLOSED

This plan originally recorded that no hooked v2.2 pair was known, so nothing
proved the hooks detection in Task 4 ever fired.

Measurement closed it. Both v2.2 fixtures already in use carry hooks, confirmed
by live `eth_call` on `getLBHooksParameters()` (`0x781a8915`):

| Pool | `getLBHooksParameters()` |
|---|---|
| `0x8573f98175d816d520248b5facf40d309b1c9cee` | `0x…0151e104964852be626ee27762712e4de521066859c9` |
| `0xcec377285abf370fdf872625d2742252656d631a` | `0x…015172ed0b6acb5c585873b3f644f99fc167c7601256` |

Low 160 bits are the hook contract address; the `0x0151` above them are the
capability flags. Task 4's live test already exercises the `warn!` path through
the first of these, so the detection is proven rather than merely written.

Two consequences, both handled in spec §7.2: hooked pools **stay in** the
convergence test (excluding them would leave v2.2 with no coverage, and hooks do
not alter the pair's own storage or events), and the apparent prevalence of
hooks on v2.2 is itself worth reporting — it makes hooked pools the norm rather
than an edge case for the downstream consumer.
