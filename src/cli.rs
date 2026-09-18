//! 命令列介面。
//!
//! 使用者可見的輸出一律英文：這個服務跑在 Debian 伺服器上，journald 在
//! 非 UTF-8 locale（常見的 `LANG=C`）下會把非 ASCII 位元組逐一轉義成
//! `\xNN`，中文訊息在 journalctl 裡會完全無法閱讀。設 locale 可以繞過，
//! 但日誌不該依賴目標機器的 locale 設定才看得懂。
//!
//! 設定一律來自 TOML 檔，這裡的旗標只處理「用哪個檔」與「檢查完就結束」，
//! 不提供覆寫個別設定值的選項——同一個值有兩個來源，就會有「現在生效的
//! 到底是哪個」這種無法從設定檔看出答案的問題。

use std::path::PathBuf;

use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "netflow-query-api",
    version,
    about = "NetFlow traffic statistics query API",
    long_about = "Read-only REST API over the statistics tables written by netflow-collector.\n\n\
                  Configuration comes from a TOML file (default: config.toml); \
                  no environment variables are read.\n\
                  Log level can be overridden with RUST_LOG, e.g. RUST_LOG=debug.",
    after_help = "Endpoints:\n  \
      GET /healthz                 liveness probe (no API key required)\n  \
      GET /api/v1/usage/today      today's usage (one or more IPs)\n  \
      GET /api/v1/usage/week       last 7 days (single IP: today + previous 6)\n  \
      GET /api/v1/usage/daily      5-minute distribution for one day (single IP + date)\n  \
      GET /api/v1/usage/exceeded   over-threshold check (also verifies source IP)\n\n\
      All /api/v1 endpoints require the header: X-API-Key: <key>\n\
      See README.md for parameters and response formats."
)]
pub struct Cli {
    /// Path to the configuration file
    #[arg(short, long, value_name = "PATH", default_value = "config.toml")]
    pub config: PathBuf,

    /// Validate the configuration file and exit; does not connect to the database or bind a port
    #[arg(long)]
    pub check_config: bool,

    /// Also verify the database connection (requires --check-config)
    #[arg(long, requires = "check_config")]
    pub check_dbconn: bool,
}
