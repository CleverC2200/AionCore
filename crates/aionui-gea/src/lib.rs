//! GEA gateway integration owned by AionCore.
//!
//! This module keeps GEA credentials and delegation tokens out of renderer and
//! agent contexts. Callers address a conversation and a tool; the service owns
//! the matching GEA session and adds gateway context at the outbound boundary.

mod error;
mod interaction_request;
mod notification;
mod routes;
mod service;
mod state;

pub use error::{GeaError, GeaErrorBody};
pub use routes::{gea_routes, gea_sales_plan_action_routes};
pub use service::GeaService;
pub use state::GeaRouterState;

pub type InteractionTurnResolver = std::sync::Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;
pub type InteractionTurnResumer = std::sync::Arc<
    dyn Fn(
            String,
            String,
            String,
            aionui_api_types::InteractionRequestReceipt,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>>
        + Send
        + Sync,
>;
