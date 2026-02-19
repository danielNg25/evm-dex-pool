pub mod pool;
pub mod v2;
pub mod v3;
pub mod erc4626;
pub(crate) mod contracts;

#[cfg(feature = "rpc")]
pub(crate) mod contracts_rpc;
#[cfg(feature = "rpc")]
mod token_info;
#[cfg(feature = "rpc")]
pub use token_info::TokenInfo;

// Core traits and types
pub use pool::{EventApplicable, PoolInterface, PoolType, PoolTypeTrait, TopicList, Topic, POOL_CREATED_TOPICS};
pub use pool::MockPool;

// V2
pub use v2::{UniswapV2Pool, V2PoolType};

// V3
pub use v3::{UniswapV3Pool, V3PoolType, Tick, TickMap};

// ERC4626
pub use erc4626::{ERC4626Pool, ERC4626Standard, VerioIP};

// Registry (optional feature)
#[cfg(feature = "registry")]
pub mod registry;

#[cfg(feature = "registry")]
pub use registry::PoolRegistry;
