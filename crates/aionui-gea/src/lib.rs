//! GEA gateway integration owned by AionCore.
//!
//! This module keeps GEA credentials and delegation tokens out of renderer and
//! agent contexts. Callers address a conversation and a tool; the service owns
//! the matching GEA session and adds gateway context at the outbound boundary.

mod error;
mod routes;
mod service;
mod state;

pub use error::{GeaError, GeaErrorBody};
pub use routes::gea_routes;
pub use service::GeaService;
pub use state::GeaRouterState;
