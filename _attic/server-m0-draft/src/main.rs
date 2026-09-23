mod api;
mod auth;
mod config;
mod error;
mod state;
mod storage;

use clap::Parser;
use std::sync::Arc;

use config::{Cli, Config};
use state::AppState;
use storage::{blob::BlobStore, db::Db};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,tower_http=warn".into()),
        )
        .init();

    let cli = Cli::parse();
    let cfg = Config::from_cli(&cli)?;

    let blobs_dir = cfg.data_dir.join("blobs");
    let tmp_dir = cfg.data_dir.join("tmp");
    let logs_dir = cfg.data_dir.join("logs");
    std::fs::create_dir_all(&blobs_dir)?;
    std::fs::create_dir_all(&tmp_dir)?;
    std::fs::create_dir_all(&logs_dir)?;

    let db = Db::open(&cfg.data_dir.join("b-artifact.db"))?;
    let blobs = BlobStore::new(&blobs_dir, &tmp_dir, cfg.compression)?;
    let state = Arc::new(AppState { cfg: cfg.clone(), db, blobs });

    if cli.bootstrap_admin {
        auth::bootstrap_admin(&state, cli.admin_password.as_deref())?;
    }

    let app = api::router(state);
    let listener = tokio::net::TcpListener::bind(&cfg.listen).await?;
    tracing::info!("b-artifact 服务端已启动: http://{}", cfg.listen);
    tracing::info!("数据目录: {}", cfg.data_dir.display());

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("收到退出信号，正在优雅关闭…");
        })
        .await?;
    Ok(())
}
