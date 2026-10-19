pub mod mcp;
pub mod openai;

use std::sync::Arc;

use crate::config::{ProviderConfig, ProviderKind};
use crate::domain::ports::LlmProvider;

pub fn build_provider(cfg: &ProviderConfig) -> Arc<dyn LlmProvider> {
    match cfg.kind {
        ProviderKind::Openai => Arc::new(openai::OpenAiProvider::new(
            &cfg.base_url,
            cfg.resolve_api_key(),
        )),
    }
}
