#[cfg(feature = "rpc")]
pub mod fetcher;
pub mod standard;
pub mod verio_ip;
use serde::{Deserialize, Serialize};

#[cfg(feature = "rpc")]
pub use fetcher::*;
pub use standard::ERC4626Standard;
pub use verio_ip::VerioIP;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ERC4626Pool {
    VerioIP,
}
