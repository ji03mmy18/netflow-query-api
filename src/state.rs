use std::sync::Arc;

use sqlx::PgPool;

use crate::config::LoadedConfig;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub config: Arc<LoadedConfig>,
}
