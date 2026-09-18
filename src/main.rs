//! NetFlow Query API
//!
//! 對 netflow-collector 寫入的統計表提供唯讀查詢，四支端點：
//!   GET /api/v1/usage/today     當日用量（單/多 IP）
//!   GET /api/v1/usage/week      歷史一週用量（單 IP，當日 + 前六日）
//!   GET /api/v1/usage/daily     一日用量分佈（單 IP + 日期，5 分鐘刻度）
//!   GET /api/v1/usage/exceeded  過量門檻檢查（額外驗來源 IP）

mod auth;
mod cli;
mod config;
mod db;
mod error;
mod models;
mod query;
mod routes;
mod state;
mod timeutil;

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use clap::Parser;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

use crate::cli::Cli;
use crate::config::LoadedConfig;
use crate::state::AppState;

type BoxError = Box<dyn std::error::Error>;

/// `main` 只負責把錯誤印成人看得懂的樣子。
///
/// 直接讓 `main` 回傳 `Result` 的話，Rust 會用 `Debug` 而非 `Display`
/// 格式化錯誤——設定檔問題會印成 `Error: BadNetwork { field: ..., .. }`
/// 這種結構傾印，而 thiserror 上寫好的訊息完全不會出現。
#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // 檢查模式沒有在啟動任何東西，錯誤前綴要跟著換，否則
    // `--check-config` 失敗時印 "startup failed" 會讓人以為服務試圖啟動過。
    let (label, result) = if cli.check_config {
        ("check failed", check(&cli).await)
    } else {
        ("startup failed", serve(&cli).await)
    };

    if let Err(error) = result {
        eprintln!("{label}: {error}");
        let mut source = error.source();
        while let Some(cause) = source {
            eprintln!("  caused by: {cause}");
            source = cause.source();
        }
        std::process::exit(1);
    }
}

/// `--check-config` / `--check-dbconn`：驗證後結束，不監聽埠號。
///
/// 刻意不初始化 tracing：這個模式的輸出是給人在終端讀的檢查報告，
/// 混入結構化日誌只會讓它變難讀。
async fn check(cli: &Cli) -> Result<(), BoxError> {
    let config = load_config(&cli.config)?;
    print_config_summary(&cli.config, &config);

    if cli.check_dbconn {
        let db = &config.config.database;
        let pool = db::connect(db).await.map_err(|error| {
            format!(
                "cannot connect to database {}:{}/{} (user={}): {error}",
                db.host, db.port, db.name, db.user
            )
        })?;

        // 連線建立成功不等於這條 session 能用（權限、search_path 等問題
        // 都要實際下一道查詢才會浮現），所以再往返一次。
        let version: String = sqlx::query_scalar("SELECT version()")
            .fetch_one(&pool)
            .await
            .map_err(|error| format!("connected, but the test query failed: {error}"))?;
        pool.close().await;

        println!();
        println!("database connection OK");
        println!("  {version}");
    }

    Ok(())
}

async fn serve(cli: &Cli) -> Result<(), BoxError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("netflow_query_api=info,tower_http=info")),
        )
        .init();

    let config = Arc::new(load_config(&cli.config)?);
    tracing::info!(path = %cli.config.display(), "config loaded");

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
            "cannot connect to database {}:{}/{} (user={}): {error}",
            db.host, db.port, db.name, db.user
        )
    })?;
    tracing::info!(host = %db.host, database = %db.name, "database connected");

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

/// 設定檔不存在是最常見的第一次執行錯誤，直接把解法寫在訊息裡。
fn load_config(path: &Path) -> Result<LoadedConfig, BoxError> {
    if !path.exists() {
        return Err(format!(
            "config file not found: {}\n\n\
             hint: copy the template, then fill in the database credentials and API keys:\n    \
             cp config.example.toml {}\n\n\
             or point --config at another path (see --help)",
            path.display(),
            path.display()
        )
        .into());
    }
    Ok(LoadedConfig::load(path)?)
}

fn print_config_summary(path: &Path, config: &LoadedConfig) {
    let c = &config.config;

    let key_names: Vec<&str> = c.auth.keys.iter().map(|k| k.name.as_str()).collect();

    let proxies = if config.trusted_proxies.is_empty() {
        "none (client IP is always the TCP peer address)".to_string()
    } else {
        join_networks(&config.trusted_proxies)
    };

    let threshold = if config.threshold_allowed_ips.is_empty() {
        "none - /api/v1/usage/exceeded is disabled".to_string()
    } else {
        join_networks(&config.threshold_allowed_ips)
    };

    // 欄寬固定，讓值在終端裡對齊成一欄，掃視時比較容易發現寫錯的那一行。
    const W: usize = 20;

    println!("config {} is valid", path.display());
    println!("  {:<W$}{}:{}", "listen", c.server.host, c.server.port);
    println!(
        "  {:<W$}{}:{}/{} (user={}, max_connections={})",
        "database", c.database.host, c.database.port, c.database.name, c.database.user,
        c.database.max_connections
    );
    println!(
        "  {:<W$}{} configured: {}",
        "api keys",
        key_names.len(),
        key_names.join(", ")
    );
    println!("  {:<W$}{proxies}", "trusted proxies");
    println!("  {:<W$}{threshold}", "threshold sources");
    println!(
        "  {:<W$}{}",
        "max ips per request", c.limits.max_ips_per_request
    );
    println!(
        "  {:<W$}{} days",
        "distribution window", c.limits.distribution_max_age_days
    );
}

fn join_networks(networks: &[ipnetwork::IpNetwork]) -> String {
    networks
        .iter()
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let interrupt = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(%error, "failed to install SIGINT handler");
            std::future::pending::<()>().await;
        }
    };

    let terminate = async {
        match signal(SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(error) => {
                tracing::error!(%error, "failed to install SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };

    tokio::select! {
        _ = interrupt => tracing::info!("SIGINT received; shutting down"),
        _ = terminate => tracing::info!("SIGTERM received; shutting down"),
    }
}
