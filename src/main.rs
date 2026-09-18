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
    // `--check-config` 失敗時印「啟動失敗」會讓人以為服務試圖啟動過。
    let (label, result) = if cli.check_config {
        ("檢查失敗", check(&cli).await)
    } else {
        ("啟動失敗", serve(&cli).await)
    };

    if let Err(error) = result {
        eprintln!("{label}：{error}");
        let mut source = error.source();
        while let Some(cause) = source {
            eprintln!("  原因：{cause}");
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
                "無法連線資料庫 {}:{}/{}（user={}）：{error}",
                db.host, db.port, db.name, db.user
            )
        })?;

        // 連線建立成功不等於這條 session 能用（權限、search_path 等問題
        // 都要實際下一道查詢才會浮現），所以再往返一次。
        let version: String = sqlx::query_scalar("SELECT version()")
            .fetch_one(&pool)
            .await
            .map_err(|error| format!("連線已建立，但查詢失敗：{error}"))?;
        pool.close().await;

        println!();
        println!("資料庫連線檢查通過");
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
            "無法連線資料庫 {}:{}/{}（user={}）：{error}",
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
            "找不到設定檔 {}\n\n\
             提示：複製範本後填入資料庫連線與 API Key：\n    \
             cp config.example.toml {}\n\n\
             或用 --config 指定其他路徑（見 --help）",
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
        "無（一律以 TCP 對端 IP 為準）".to_string()
    } else {
        config
            .trusted_proxies
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };

    let threshold = if config.threshold_allowed_ips.is_empty() {
        "無 → 門檻檢查端點停用".to_string()
    } else {
        config
            .threshold_allowed_ips
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };

    println!("設定檔 {} 檢查通過", path.display());
    println!("  監聽位址        {}:{}", c.server.host, c.server.port);
    println!(
        "  資料庫          {}:{}/{}（user={}, max_connections={}）",
        c.database.host, c.database.port, c.database.name, c.database.user, c.database.max_connections
    );
    println!("  API Key         {} 把：{}", key_names.len(), key_names.join(", "));
    println!("  受信任代理      {proxies}");
    println!("  門檻檢查來源    {threshold}");
    println!("  單次 IP 上限    {}", c.limits.max_ips_per_request);
    println!(
        "  分佈回溯上限    {} 天",
        c.limits.distribution_max_age_days
    );
}

async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::error!(%error, "failed to install ctrl-c handler");
    }
    tracing::info!("shutdown signal received");
}
