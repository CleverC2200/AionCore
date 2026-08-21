//! Feishu approval gateway owned by AionCore.
//!
//! The renderer receives normalized approval data and never reads lark-cli
//! profiles or credentials directly.

mod error;
mod routes;
mod service;
mod state;

pub use error::{ApprovalError, ApprovalUpstreamError};
pub use routes::{approval_action_routes, approval_read_routes, approval_routes};
pub use service::ApprovalService;
pub use state::ApprovalRouterState;
