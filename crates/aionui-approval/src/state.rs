use std::sync::Arc;

use crate::ApprovalService;

#[derive(Clone)]
pub struct ApprovalRouterState {
    pub service: Arc<ApprovalService>,
}

impl ApprovalRouterState {
    pub fn new(service: ApprovalService) -> Self {
        Self {
            service: Arc::new(service),
        }
    }
}
