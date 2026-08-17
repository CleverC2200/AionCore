use std::sync::Arc;

use crate::service::GeaService;

#[derive(Clone)]
pub struct GeaRouterState {
    pub service: Arc<GeaService>,
}

impl GeaRouterState {
    pub fn new(service: GeaService) -> Self {
        Self {
            service: Arc::new(service),
        }
    }
}
