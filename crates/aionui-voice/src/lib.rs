#![warn(clippy::disallowed_types)]

//! Managed realtime voice sessions.

mod agent;
mod error;
mod provider;
mod routes;
mod service;
mod state;

pub use agent::{VoiceAgentError, VoiceConversationAgent};
pub use error::VoiceError;
pub use provider::{
    ManagedVoiceBackend, ProviderError, VoiceProviderRegistry, VoiceProviderSession, provider_from_environment,
};
pub use routes::{voice_capability_routes, voice_configuration_action_routes, voice_session_routes};
pub use service::VoiceService;
pub use state::VoiceRouterState;
