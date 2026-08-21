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
    let logs = provider.get_logs(&filter).await?;
    println!("[{label}] applying {} logs", logs.len());
    assert!(!logs.is_empty(), "{label}: no logs in range — widen REPLAY_BLOCKS");

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

### Task 8: Composition fee handling

**Files:**
- Modify: `src/lb/pool.rs`
- Test: `tests/lb_convergence.rs` (rerun)

**Interfaces:**
- Consumes: `ILBPair::CompositionFees` binding (Task 2).
- Produces: `LBPool::topics()` includes `CompositionFees`; `apply_log` handles it.

**Only do this task if Task 7 Step 2 failed on bin reserves.** If convergence already passed, skip to Task 9 and record why.

Composition fees are charged when liquidity is added to the active bin at a ratio different from the bin's current composition. The fee is credited to that bin's reserves, so ignoring the event leaves replayed reserves short.

- [ ] **Step 1: Register the topic**

In `impl TopicList for LBPool` in `src/lb/pool.rs`, add to the `topics()` vector:

```rust
            ILBPair::CompositionFees::SIGNATURE_HASH,
```

Leave `profitable_topics()` unchanged — a composition fee is not a trade opportunity.

- [ ] **Step 2: Handle the event**

Add an arm to `apply_log`, before the catch-all `_ =>`:

```rust
            Some(&ILBPair::CompositionFees::SIGNATURE_HASH) => {
                let data: ILBPair::CompositionFees = event.log_decode()?.inner.data;
                let id: u32 = data.id.to();
                // totalFees are credited to the bin; protocolFees are carved
                // out of them and are not part of the bin's reserves.
                let (total_x, total_y) = decode_amounts(data.totalFees);
                let (proto_x, proto_y) = decode_amounts(data.protocolFees);
                let (rx, ry) = self.bins.get(&id).copied().unwrap_or((0, 0));
                self.update_bin(
                    id,
                    rx.saturating_add(total_x).saturating_sub(proto_x),
                    ry.saturating_add(total_y).saturating_sub(proto_y),
                );
                self.last_updated = Self::log_timestamp(event);
                Ok(())
            }
```

- [ ] **Step 3: Rerun the convergence test**

Run: `cargo test --features collector test_lb_convergence -- --ignored --nocapture`
Expected: PASS.

If bins still diverge, the remaining candidate is flash-loan fees, which are credited to bins via a `FlashLoan` event this implementation does not observe. Add that binding and handler the same way before loosening anything.

- [ ] **Step 4: Verify both builds**

Run: `cargo check && cargo check --features collector`
Expected: both succeed.

- [ ] **Step 5: Commit**

```bash
git add src/lb/pool.rs
git commit -m "fix(lb): apply composition fees to bin reserves

CompositionFees was neither registered as a topic nor handled, so fees
credited to the active bin on unbalanced deposits were missing from
replayed state."
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
                let amount_in: u128 = d.amountIn.try_into().unwrap_or(u128::MAX);
                let amount_out: u128 = d.amountOut.try_into().unwrap_or(u128::MAX);

                let (rx, ry) = self.bins.get(&id).copied().unwrap_or((0, 0));
                // swapForY: X goes in, Y comes out.
                let (new_rx, new_ry) = if d.swapForY {
                    (rx.saturating_add(amount_in), ry.saturating_sub(amount_out))
                } else {
                    (rx.saturating_sub(amount_out), ry.saturating_add(amount_in))
                };
                self.update_bin(id, new_rx, new_ry);

                self.active_id = id;
                self.volatility_accumulator = d.volatilityAccumulated.to();
                self.id_reference = id;
                let now = Self::log_timestamp(event);
                self.time_of_last_update = now;
                self.last_updated = now;
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

- [ ] **Step 1: Add v2.0 convergence**

Append to `tests/lb_convergence.rs`:

```rust
const V20_POOL: Address = address!("18332988456C4Bd9ABa6698ec748b331516F5A14");

/// The v2.0 pool is nearly dormant — measured at 12 logs across 20,000
/// blocks, with none at all in the most recent 12,000. A head-derived range
/// would find zero logs and trip the test's own `!logs.is_empty()` assertion
/// every run, so this fixture pins a historical window known to contain
/// Swaps. Re-pin if archive retention ever drops it; the failure is loud.
const V20_RANGE: (u64, u64) = (93_327_570, 93_335_570);

#[tokio::test]
#[ignore]
async fn test_lb_convergence_v20() -> Result<()> {
    converge_one(V20_POOL, "v2.0", Some(V20_RANGE)).await
}
```

`V20_RANGE` spans 8,000 blocks, which exceeds this endpoint's 2048-block
`eth_getLogs` cap — the log fetch for a pinned range must be **chunked into
2048-block windows** and concatenated in order. Apply the same chunking in
`converge_one` so the head-derived path stays correct if `REPLAY_BLOCKS` grows.

- [ ] **Step 2: Run it and interpret the result**

Run: `cargo test --features collector test_lb_convergence_v20 -- --ignored --nocapture`

- **Passes** → v2.0's `getBin()` excludes fees exactly as `amountIn` does, and the straightforward accounting is correct. Record this in the commit message; it settles the spec's §6.3 open question.
- **Fails on bin reserves, replayed consistently lower than fetched** → v2.0's `getBin()` includes fees that `amountIn` excludes. Add the `fees` field into the input side of the v2.0 `Swap` arm, splitting it by direction, and rerun.
- **Fails only on `time_of_last_update`** → v2.0's `feeParameters().time` has different semantics from v2.1+'s `timeOfLastUpdate`. Investigate before adjusting.

If the pool is too quiet to produce logs, raise `REPLAY_BLOCKS` rather than dropping the assertion.

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
