use crate::contracts::IERC4626;
use crate::pool::base::{EventApplicable, PoolInterface, PoolType, PoolTypeTrait, TopicList};
use crate::erc4626::{ERC4626Pool, ERC4626Standard};
use alloy::sol_types::SolEvent;
use alloy::{
    primitives::{Address, FixedBytes, U256},
    rpc::types::Log,
};
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::any::Any;
use std::fmt;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerioIP {
    base: ERC4626Standard,
}

impl VerioIP {
    pub fn new(
        address: Address,
        vault_token: Address,
        asset_token: Address,
        vault_reserve: U256,
        asset_reserve: U256,
        deposit_fee: u32,
        withdraw_fee: u32,
    ) -> Self {
        Self {
            base: ERC4626Standard::new(
                address,
                vault_token,
                asset_token,
                vault_reserve,
                asset_reserve,
                deposit_fee,
                withdraw_fee,
            ),
        }
    }
}

impl PoolInterface for VerioIP {
    fn address(&self) -> Address {
        self.base.address
    }

    fn calculate_output(&self, token_in: &Address, amount_in: U256) -> Result<U256> {
        self.base.calculate_output(token_in, amount_in)
    }

    fn calculate_input(&self, token_out: &Address, amount_out: U256) -> Result<U256> {
        self.base.calculate_input(token_out, amount_out)
    }

    fn apply_swap(
        &mut self,
        _token_in: &Address,
        _amount_in: U256,
        _amount_out: U256,
    ) -> Result<()> {
        Err(anyhow!("Not implemented"))
    }

    fn tokens(&self) -> (Address, Address) {
        self.base.tokens()
    }

    fn fee(&self) -> f64 {
        self.base.fee()
    }

    fn fee_raw(&self) -> u64 {
        self.base.fee_raw()
    }

    fn id(&self) -> String {
        self.base.id()
    }

    fn contains_token(&self, token: &Address) -> bool {
        self.base.contains_token(token)
    }

    fn clone_box(&self) -> Box<dyn PoolInterface + Send + Sync> {
        Box::new(self.clone())
    }

    fn log_summary(&self) -> String {
        self.base.log_summary()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

impl PoolTypeTrait for VerioIP {
    fn pool_type(&self) -> PoolType {
        PoolType::ERC4626(ERC4626Pool::VerioIP)
    }
}

impl EventApplicable for VerioIP {
    fn apply_log(&mut self, log: &Log) -> Result<()> {
        match log.topic0() {
            Some(&IERC4626::Deposit::SIGNATURE_HASH) => {
                let deposit_data: IERC4626::Deposit = log.log_decode()?.inner.data;
                self.base.vault_reserve += deposit_data.shares;
                self.base.asset_reserve += deposit_data.assets;
            }
            Some(&IERC4626::Withdraw::SIGNATURE_HASH) => {
                let withdraw_data: IERC4626::Withdraw = log.log_decode()?.inner.data;
                self.base.vault_reserve -= withdraw_data.shares;
                self.base.asset_reserve -= withdraw_data.assets;
            }
            _ => return Ok(()),
        }
        Ok(())
    }
}

impl TopicList for VerioIP {
    fn topics() -> Vec<FixedBytes<32>> {
        vec![
            IERC4626::Deposit::SIGNATURE_HASH,
            IERC4626::Withdraw::SIGNATURE_HASH,
        ]
    }

    fn profitable_topics() -> Vec<FixedBytes<32>> {
        vec![
            IERC4626::Deposit::SIGNATURE_HASH,
            IERC4626::Withdraw::SIGNATURE_HASH,
        ]
    }
}

impl fmt::Display for VerioIP {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Verio IP Pool {} - {} (reserves: {}, {})",
            self.base.vault_token,
            self.base.asset_token,
            self.base.vault_reserve,
            self.base.asset_reserve
        )
    }
}
