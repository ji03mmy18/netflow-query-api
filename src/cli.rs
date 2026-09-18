//! 命令列介面。
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
    about = "NetFlow 流量統計查詢 API",
    long_about = "對 netflow-collector 寫入的統計表提供唯讀查詢的 REST API。\n\n\
                  設定一律來自 TOML 檔（預設 config.toml），不讀環境變數。\n\
                  日誌層級可用 RUST_LOG 覆寫，例如 RUST_LOG=debug。",
    after_help = "端點一覽：\n  \
      GET /healthz                 服務存活探測（不需 API Key）\n  \
      GET /api/v1/usage/today      當日用量（單/多 IP）\n  \
      GET /api/v1/usage/week       歷史一週用量（單 IP，當日 + 前六日）\n  \
      GET /api/v1/usage/daily      一日 5 分鐘分佈（單 IP + 日期）\n  \
      GET /api/v1/usage/exceeded   過量門檻檢查（額外驗來源 IP）\n\n\
      所有 /api/v1 端點需帶 header：X-API-Key: <key>\n\
      參數與回應格式詳見 README.md"
)]
pub struct Cli {
    /// 設定檔路徑
    #[arg(short, long, value_name = "PATH", default_value = "config.toml")]
    pub config: PathBuf,

    /// 只驗證設定檔後結束，不連線資料庫、不監聽埠號
    #[arg(long)]
    pub check_config: bool,

    /// 連同資料庫連線一起驗證（須搭配 --check-config）
    #[arg(long, requires = "check_config")]
    pub check_dbconn: bool,
}
