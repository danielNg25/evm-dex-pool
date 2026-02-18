use alloy::{
    primitives::{Address, FixedBytes, U256},
    rpc::types::Log,
};
use anyhow::{anyhow, Ok, Result};
use std::any::Any;

use crate::pool::base::TopicList;
use crate::pool::base::{EventApplicable, PoolInterface, PoolType, PoolTypeTrait};

#[derive(Debug, Clone)]
pub struct MockPool {
    address: Address,
    token0: Address,
    token1: Address,
    reserve0: U256,
    reserve1: U256,
    fee: f64,
    pool_type: PoolType,
    last_updated: u64,
}

impl MockPool {
    pub fn new(
        address: Address,
        token0: Address,
        token1: Address,
        reserve0: U256,
        reserve1: U256,
        fee: f64,
        pool_type: PoolType,
    ) -> Self {
        let current_time = chrono::Utc::now().timestamp() as u64;
        Self {
            address,
            token0,
            token1,
            reserve0,
            reserve1,
            fee,
            pool_type,
            last_updated: current_time,
        }
    }

    /// Create a default boxed MockPool for testing
    pub fn new_boxed() -> Box<dyn PoolInterface + Send + Sync> {
        Box::new(Self::new_v2(
            Address::ZERO,
            Address::ZERO,
            Address::ZERO,
            U256::from(1000000u64),
            U256::from(1000000u64),
        ))
    }

    pub fn new_v2(
        address: Address,
        token0: Address,
        token1: Address,
        reserve0: U256,
        reserve1: U256,
    ) -> Self {
        Self::new(
            address,
            token0,
            token1,
            reserve0,
            reserve1,
            0.003,
            PoolType::UniswapV2,
        )
    }

    pub fn new_v3(
        address: Address,
        token0: Address,
        token1: Address,
        reserve0: U256,
        reserve1: U256,
        fee: f64,
    ) -> Self {
        Self::new(
            address,
            token0,
            token1,
            reserve0,
            reserve1,
            fee,
            PoolType::UniswapV3,
        )
    }

    pub fn set_reserves(&mut self, reserve0: U256, reserve1: U256) {
        self.reserve0 = reserve0;
        self.reserve1 = reserve1;
        self.last_updated = chrono::Utc::now().timestamp() as u64;
    }
}

impl PoolInterface for MockPool {
    fn calculate_output(&self, token_in: &Address, amount_in: U256) -> Result<U256> {
        if token_in == &self.token0 {
            let amount_in_with_fee = amount_in * U256::from(997);
            let numerator = amount_in_with_fee * self.reserve1;
            let denominator = self.reserve0 * U256::from(1000) + amount_in_with_fee;
            Ok(numerator / denominator)
        } else if token_in == &self.token1 {
            let amount_in_with_fee = amount_in * U256::from(997);
            let numerator = amount_in_with_fee * self.reserve0;
            let denominator = self.reserve1 * U256::from(1000) + amount_in_with_fee;
            Ok(numerator / denominator)
        } else {
            Err(anyhow!("Token not in pool"))
        }
    }

    fn calculate_input(&self, token_out: &Address, amount_out: U256) -> Result<U256> {
        if token_out == &self.token0 {
            let numerator = self.reserve1 * amount_out * U256::from(1000);
            let denominator = (self.reserve0 - amount_out) * U256::from(997);
            Ok((numerator / denominator) + U256::from(1))
        } else if token_out == &self.token1 {
            let numerator = self.reserve0 * amount_out * U256::from(1000);
            let denominator = (self.reserve1 - amount_out) * U256::from(997);
            Ok((numerator / denominator) + U256::from(1))
        } else {
            Err(anyhow!("Token not in pool"))
        }
    }

    fn apply_swap(&mut self, token_in: &Address, amount_in: U256, amount_out: U256) -> Result<()> {
        if token_in == &self.token0 {
            if amount_out >= self.reserve1 {
                return Err(anyhow!("Insufficient liquidity for swap"));
            }
            self.reserve0 += amount_in;
            self.reserve1 -= amount_out;
        } else if token_in == &self.token1 {
            if amount_out >= self.reserve0 {
                return Err(anyhow!("Insufficient liquidity for swap"));
            }
            self.reserve1 += amount_in;
            self.reserve0 -= amount_out;
        } else {
            return Err(anyhow!("Token not in pool"));
        }

        self.last_updated = chrono::Utc::now().timestamp() as u64;
        Ok(())
    }

    fn address(&self) -> Address {
        self.address
    }

    fn tokens(&self) -> (Address, Address) {
        (self.token0, self.token1)
    }

    fn log_summary(&self) -> String {
        format!("Mock Pool")
    }

    fn fee(&self) -> f64 {
        self.fee
    }

    fn fee_raw(&self) -> u64 {
        self.fee as u64
    }

    fn id(&self) -> String {
        format!(
            "mock-{}-{}-{}-{:?}",
            self.address, self.token0, self.token1, self.pool_type
        )
    }

    fn contains_token(&self, token: &Address) -> bool {
        *token == self.token0 || *token == self.token1
    }

    fn clone_box(&self) -> Box<dyn PoolInterface + Send + Sync> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

impl PoolTypeTrait for MockPool {
    fn pool_type(&self) -> PoolType {
        self.pool_type
    }
}

impl EventApplicable for MockPool {
    fn apply_log(&mut self, _event: &Log) -> Result<()> {
        Ok(())
    }
}

impl TopicList for MockPool {
    fn topics() -> Vec<FixedBytes<32>> {
        vec![]
    }

    fn profitable_topics() -> Vec<FixedBytes<32>> {
        vec![]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mock_pool() {
        let token0 = Address::ZERO;
        let token1 = Address::repeat_byte(1);
        let pool_addr = Address::repeat_byte(2);

        let pool = MockPool::new_v2(
            pool_addr,
            token0,
            token1,
            U256::from(1000000),
            U256::from(1000000),
        );

        assert_eq!(pool.address(), pool_addr);
        assert_eq!(pool.tokens(), (token0, token1));
        assert_eq!(pool.pool_type(), PoolType::UniswapV2);
        assert_eq!(pool.fee(), 0.003);

        let amount_out = pool.calculate_output(&token0, U256::from(1000)).unwrap();
        assert!(amount_out > U256::ZERO);
    }
}
