use std::sync::Arc;

use crate::service::GeaService;

#[derive(Clone)]
pub struct GeaRouterState {
    pub service: Arc<GeaService>,
    pub trusted_submit_secret: Option<Arc<str>>,
}

impl GeaRouterState {
    pub fn new(service: GeaService) -> Self {
        Self {
            service: Arc::new(service),
            trusted_submit_secret: None,
        }
    }

    pub fn with_trusted_submit_secret(mut self, secret: Option<Arc<str>>) -> Self {
        self.trusted_submit_secret = secret;
        self
    }
}
