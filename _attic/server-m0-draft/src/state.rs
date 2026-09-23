use std::sync::Arc;

use crate::config::Config;
use crate::storage::{blob::BlobStore, db::Db};

pub struct AppState {
    pub cfg: Config,
    pub db: Db,
    pub blobs: BlobStore,
}

pub type SharedState = Arc<AppState>;
