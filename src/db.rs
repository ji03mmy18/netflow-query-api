//! 資料庫連線池。
//!
//! 本服務只做 SELECT，建議以唯讀帳號連線（見 config.example.toml）。

use std::time::Duration;

use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

use crate::config::DatabaseConfig;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// 啟動時就實際連一次，設定錯誤在啟動階段就會暴露。
///
/// `acquire_timeout` 必須明確設定：預設值下，資料庫位址或埠號寫錯會讓
/// sqlx 持續重試，啟動看起來像「卡住」而不是「設定錯了」——這是最難
/// 診斷的一種失敗。10 秒足夠涵蓋正常的連線建立與認證。
pub async fn connect(config: &DatabaseConfig) -> Result<PgPool, sqlx::Error> {
    let options = PgConnectOptions::new()
        .host(&config.host)
        .port(config.port)
        .database(&config.name)
        .username(&config.user)
        .password(&config.password);

    PgPoolOptions::new()
        .max_connections(config.max_connections)
        .acquire_timeout(CONNECT_TIMEOUT)
        .connect_with(options)
        .await
}
