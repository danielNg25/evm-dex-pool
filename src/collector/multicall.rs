use alloy::primitives::Address;
use alloy::providers::MULTICALL3_ADDRESS;

/// Custom multicall3 addresses for chains where the standard
/// `0xcA11bde05977b3631167028862bE2a173976CA11` is not deployed.
///
/// Add entries here when a chain needs a non-standard multicall3 address.
fn get_custom_multicall_address(chain_id: u64) -> Option<Address> {
    match chain_id {
        50 => Some(
            "0x0c9A8dB3B6C58bC02b8473167b0062b543F3ED7f"
                .parse()
                .unwrap(),
        ),
        _ => None,
    }
}

/// Resolve the multicall address to use for a given chain.
///
/// Priority:
/// 1. Caller-provided override (from config)
/// 2. Chain-specific custom address (hardcoded in this module)
/// 3. Standard MULTICALL3 (`0xcA11bde05977b3631167028862bE2a173976CA11`)
pub fn resolve_multicall_address(chain_id: u64, override_address: Option<Address>) -> Address {
    override_address
        .or_else(|| get_custom_multicall_address(chain_id))
        .unwrap_or(MULTICALL3_ADDRESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_override_takes_priority() {
        let custom: Address = "0x0000000000000000000000000000000000000001"
            .parse()
            .unwrap();
        assert_eq!(resolve_multicall_address(1, Some(custom)), custom);
    }

    #[test]
    fn test_falls_back_to_multicall3() {
        assert_eq!(resolve_multicall_address(1, None), MULTICALL3_ADDRESS);
    }
}
