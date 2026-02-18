pub mod standard;
pub mod verio_ip;
#[cfg(feature = "rpc")]
pub mod fetcher;
use serde::{Deserialize, Serialize};

pub use standard::ERC4626Standard;
pub use verio_ip::VerioIP;
#[cfg(feature = "rpc")]
pub use fetcher::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ERC4626Pool {
    VerioIP,
}
