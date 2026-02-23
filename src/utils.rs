use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::client::RpcClient;
use alloy::transports::http::Http;
use alloy::transports::layers::FallbackLayer;
use std::num::NonZeroUsize;
use tower::ServiceBuilder;
use url::Url;

/// Create an RPC provider with fallback layer across multiple RPC URLs.
pub fn create_fallback_provider(
    rpc_urls: &[String],
) -> impl Provider + Clone + 'static {
    let rpc_len = rpc_urls.len();
    let fallback_layer =
        FallbackLayer::default().with_active_transport_count(NonZeroUsize::new(rpc_len).unwrap());

    let transports = rpc_urls
        .iter()
        .map(|url| Http::new(Url::parse(url).unwrap()))
        .collect::<Vec<_>>();

    let transport = ServiceBuilder::new()
        .layer(fallback_layer)
        .service(transports);
    let client = RpcClient::builder().transport(transport, false);
    ProviderBuilder::new().connect_client(client)
}
