use crate::contracts::{IAlgebraFactory, IUniswapV2Factory, IUniswapV3Factory, IVeloPoolFactory};
use crate::erc4626::ERC4626Pool;
use crate::erc4626::VerioIP;
use crate::lb::LBPool;
use crate::v2::UniswapV2Pool;
use crate::v3::UniswapV3Pool;
use alloy::sol_types::SolEvent;
use alloy::{
    primitives::{Address, FixedBytes, U256},
    rpc::types::Log,
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::any::Any;

pub type Topic = FixedBytes<32>;

/// Interface for pool operations, implemented by all pool types (V2, V3, etc.)
pub trait PoolTypeTrait: Send + Sync {
    fn pool_type(&self) -> PoolType;
}

/// Context for a quote whose result depends on when the swap executes.
///
/// Only pool types with time-dependent state consult this — currently only
/// Trader Joe LB, whose variable fee decays against `timeOfLastUpdate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuoteContext {
    /// Block timestamp the swap is expected to execute at, in seconds.
    pub timestamp: u64,
}

pub trait PoolInterface: std::fmt::Debug + Send + Sync + PoolTypeTrait + EventApplicable {
    /// Calculate output amount for a swap given an input amount and token
    fn calculate_output(&self, token_in: &Address, amount_in: U256) -> Result<U256>;

    /// Calculate input amount for a swap given an output amount and token
    fn calculate_input(&self, token_out: &Address, amount_out: U256) -> Result<U256>;

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

    /// Apply a swap to the pool state
    fn apply_swap(&mut self, token_in: &Address, amount_in: U256, amount_out: U256) -> Result<()>;

    /// Get the pool address
    fn address(&self) -> Address;

    /// Get the tokens in the pool
    fn tokens(&self) -> (Address, Address);

    /// Get token0 of the pool
    fn token0(&self) -> Address {
        self.tokens().0
    }

    /// Get token1 of the pool
    fn token1(&self) -> Address {
        self.tokens().1
    }

    /// Get the pool fee as a fraction (e.g., 0.003 for 0.3%)
    fn fee(&self) -> f64;

    fn fee_raw(&self) -> u64;

    /// Get a unique identifier for the pool
    fn id(&self) -> String;

    /// Check if the pool contains a token
    fn contains_token(&self, token: &Address) -> bool;

    /// Clone the pool interface to a box
    fn clone_box(&self) -> Box<dyn PoolInterface + Send + Sync>;

    /// Log summary of the pool
    fn log_summary(&self) -> String;

    /// Helper method for downcasting
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

impl dyn PoolInterface + Send + Sync {
    /// Downcast to a concrete pool type
    pub fn downcast_ref<T: 'static>(&self) -> Option<&T> {
        self.as_any().downcast_ref::<T>()
    }

    /// Downcast to a concrete pool type (mutable)
    pub fn downcast_mut<T: 'static>(&mut self) -> Option<&mut T> {
        self.as_any_mut().downcast_mut::<T>()
    }
}

/// Pool type enum
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PoolType {
    /// Uniswap V2-compatible pool (constant product formula)
    UniswapV2,
    /// Uniswap V3-compatible pool (concentrated liquidity)
    UniswapV3,
    /// ERC4626-compatible pool
    ERC4626(ERC4626Pool),
    /// TraderJoe Liquidity Book pool (bin-based AMM)
    TraderJoeLB,
}

impl std::fmt::Display for PoolType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PoolType::UniswapV2 => write!(f, "uniswap_v2"),
            PoolType::UniswapV3 => write!(f, "uniswap_v3"),
            PoolType::ERC4626(ERC4626Pool::VerioIP) => write!(f, "verio_ip"),
            PoolType::TraderJoeLB => write!(f, "traderjoe_liquidity_book"),
        }
    }
}

impl From<PoolType> for String {
    fn from(pool_type: PoolType) -> Self {
        pool_type.to_string()
    }
}

impl Default for PoolType {
    fn default() -> Self {
        PoolType::UniswapV2
    }
}

impl PoolType {
    pub fn topics(&self) -> Vec<FixedBytes<32>> {
        match self {
            Self::UniswapV2 => UniswapV2Pool::topics(),
            Self::UniswapV3 => UniswapV3Pool::topics(),
            Self::ERC4626(ERC4626Pool::VerioIP) => VerioIP::topics(),
            Self::TraderJoeLB => LBPool::topics(),
        }
    }

    pub fn profitable_topics(&self) -> Vec<FixedBytes<32>> {
        match self {
            Self::UniswapV2 => UniswapV2Pool::profitable_topics(),
            Self::UniswapV3 => UniswapV3Pool::profitable_topics(),
            Self::ERC4626(ERC4626Pool::VerioIP) => VerioIP::profitable_topics(),
            Self::TraderJoeLB => LBPool::profitable_topics(),
        }
    }
}

/// Trait for applying events to pool state
pub trait EventApplicable {
    /// Apply an event to update the pool state
    fn apply_log(&mut self, event: &Log) -> Result<()>;
}

pub trait TopicList {
    fn topics() -> Vec<Topic>;

    fn profitable_topics() -> Vec<Topic>;
}

pub const POOL_CREATED_TOPICS: &[Topic] = &[
    IUniswapV2Factory::PairCreated::SIGNATURE_HASH,
    IUniswapV3Factory::PoolCreated::SIGNATURE_HASH,
    IAlgebraFactory::Pool::SIGNATURE_HASH,
    IVeloPoolFactory::PoolCreated::SIGNATURE_HASH,
];
