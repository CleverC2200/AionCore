use std::sync::Arc;

use crate::VoiceConversationAgent;
use crate::provider::VoiceProviderRegistry;
use crate::service::VoiceService;

#[derive(Clone)]
pub struct VoiceRouterState {
    pub service: Arc<VoiceService>,
}

impl VoiceRouterState {
    pub fn new(service: VoiceService) -> Self {
        Self {
            service: Arc::new(service),
        }
    }

    pub fn from_environment(agent: Arc<dyn VoiceConversationAgent>) -> Self {
        Self::new(VoiceService::new(crate::provider::provider_from_environment(), agent))
    }

    pub fn with_registry(registry: Arc<VoiceProviderRegistry>, agent: Arc<dyn VoiceConversationAgent>) -> Self {
        Self::new(VoiceService::with_registry(registry, agent))
    }
}
