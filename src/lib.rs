pub(crate) mod contracts;
pub mod erc4626;
pub mod pool;
pub mod v2;
pub mod v3;

#[cfg(feature = "rpc")]
pub(crate) mod contracts_rpc;
#[cfg(feature = "rpc")]
mod token_info;
#[cfg(feature = "rpc")]
pub use token_info::TokenInfo;
#[cfg(feature = "rpc")]
pub mod utils;
#[cfg(feature = "rpc")]
pub use utils::create_fallback_provider;

// Core traits and types
pub use pool::MockPool;
pub use pool::{
    EventApplicable, PoolInterface, PoolType, PoolTypeTrait, Topic, TopicList, POOL_CREATED_TOPICS,
};

// V2
pub use v2::{UniswapV2Pool, V2PoolType};

// V3
pub use v3::{Tick, TickMap, UniswapV3Pool, V3PoolType};

// ERC4626
pub use erc4626::{ERC4626Pool, ERC4626Standard, VerioIP};

// Registry (optional feature)
#[cfg(feature = "registry")]
pub mod registry;

#[cfg(feature = "registry")]
pub use registry::PoolRegistry;

// Collector (optional feature)
#[cfg(feature = "collector")]
pub mod collector;
