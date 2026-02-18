use crate::contracts_rpc::RpcIVerioIP as IVerioIP;
use crate::erc4626::{ERC4626Pool, VerioIP};
use crate::{PoolInterface, TokenInfo};
use alloy::eips::BlockId;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use anyhow::Result;
use std::str::FromStr;
use std::sync::Arc;

pub async fn fetch_erc4626_pool<P: Provider + Send + Sync, T: TokenInfo>(
    provider: &Arc<P>,
    pool_type: ERC4626Pool,
    pool_address: Address,
    block_number: BlockId,
    token_info: &T,
) -> Result<Box<dyn PoolInterface>> {
    match pool_type {
        ERC4626Pool::VerioIP => {
            let pool =
                fetch_verio_ip_pool(provider, pool_address, block_number, token_info).await?;
            Ok(Box::new(pool))
        }
    }
}

pub async fn fetch_verio_ip_pool<P: Provider + Send + Sync, T: TokenInfo>(
    provider: &Arc<P>,
    pool_address: Address,
    _block_number: BlockId,
    _token_info: &T,
) -> Result<VerioIP> {
    let vault_token = Address::from_str(&"0x5267F7eE069CEB3D8F1c760c215569b79d0685aD").unwrap();
    let asset_token = Address::ZERO;
    let _vault = IVerioIP::new(pool_address, &provider);
    let (vault_reserve, asset_reserve) = (U256::ZERO, U256::ZERO);
    let deposit_fee = 0;
    let withdraw_fee = 0;

    Ok(VerioIP::new(
        pool_address,
        vault_token,
        asset_token,
        vault_reserve,
        asset_reserve,
        deposit_fee,
        withdraw_fee,
    ))
}
