pub mod types;

use std::pin::Pin;

use async_trait::async_trait;
use futures_core::Stream;
use tokio_util::sync::CancellationToken;

pub use types::*;

pub type ProviderStream = Pin<Box<dyn Stream<Item = Result<ModelEvent, ModelError>> + Send>>;

#[async_trait]
pub trait ModelProvider: Send + Sync {
    fn id(&self) -> &str;

    fn capabilities(&self) -> ProviderCapabilities;

    async fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ProviderStream, ModelError>;
}
