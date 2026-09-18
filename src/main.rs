//! NetFlow Query API
//!
//! 對 netflow-collector 寫入的統計表提供唯讀查詢，四支端點：
//!   GET /api/v1/usage/today     當日用量（單/多 IP）
//!   GET /api/v1/usage/week      歷史一週用量（單 IP，當日 + 前六日）
//!   GET /api/v1/usage/daily     一日用量分佈（單 IP + 日期，5 分鐘刻度）
//!   GET /api/v1/usage/exceeded  過量門檻檢查（額外驗來源 IP）

mod auth;
mod config;
mod db;
mod error;
mod models;
mod query;
mod routes;
mod state;
mod timeutil;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

use crate::config::LoadedConfig;
use crate::state::AppState;

const DEFAULT_CONFIG_PATH: &str = "config.toml";

/// `main` 只負責把錯誤印成人看得懂的樣子。
///
/// 直接讓 `main` 回傳 `Result` 的話，Rust 會用 `Debug` 而非 `Display`
/// 格式化錯誤——設定檔問題會印成 `Error: BadNetwork { field: ..., .. }`
/// 這種結構傾印，而 thiserror 上寫好的訊息完全不會出現。
#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("啟動失敗：{error}");
        let mut source = error.source();
        while let Some(cause) = source {
            eprintln!("  原因：{cause}");
            source = cause.source();
        }
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("netflow_query_api=info,tower_http=info")),
        )
        .init();

    let config_path: PathBuf = std::env::args()
        .nth(1)
        .unwrap_or_else(|| DEFAULT_CONFIG_PATH.to_string())
        .into();

    let config = Arc::new(LoadedConfig::load(&config_path)?);
    tracing::info!(path = %config_path.display(), "config loaded");

    if !config.threshold_check_enabled() {
        tracing::warn!(
            "threshold_check.allowed_ips is empty; GET /api/v1/usage/exceeded will reject all requests"
        );
    }
    if config.trusted_proxies.is_empty() {
        tracing::info!("no trusted proxies configured; client IP is always the TCP peer address");
    }

    // sqlx 的 PoolTimedOut 不帶底層原因（連線被拒？認證失敗？），
    // 訊息裡補上實際連線目標，才知道該去檢查什麼。
    let db = &config.config.database;
    let pool = db::connect(db).await.map_err(|error| {
        format!(
            "無法連線資料庫 {}:{}/{}（user={}）：{error}",
            db.host, db.port, db.name, db.user
        )
    })?;
    tracing::info!(
        host = %config.config.database.host,
        database = %config.config.database.name,
        "database connected"
    );

    let addr: SocketAddr = format!("{}:{}", config.config.server.host, config.config.server.port)
        .parse()?;

    let app = routes::router(AppState {
        pool: pool.clone(),
        config: Arc::clone(&config),
    })
    .layer(TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "listening");

    // ConnectInfo 是門檻檢查端點判斷來源 IP 的唯一依據，必須用
    // into_make_service_with_connect_info 啟動，否則該端點會一律拒絕。
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;

    pool.close().await;
    tracing::info!("shutdown complete");
    Ok(())
}

async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::error!(%error, "failed to install ctrl-c handler");
    }
    tracing::info!("shutdown signal received");
}
