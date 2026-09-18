//! API 回應結構。
//!
//! `bytes` 一律是 `ext_rx_bytes + ext_tx_bytes`，即「對外雙向流量合計」。
//! 刻意不回傳 `intra_rx_bytes` / `intra_tx_bytes`：schema 註解已說明
//! intra_* 的母體結構性不完整，而且兩端都在監控清單時同一份流量會被
//! 重複計入，不適合當作用量數字對外提供。

use serde::Serialize;

/// 單一 IP 的一個用量數字。
#[derive(Debug, Serialize)]
pub struct IpUsage {
    pub ip: String,
    pub bytes: i64,
}

/// 一、當日用量查詢
#[derive(Debug, Serialize)]
pub struct TodayUsage {
    /// 台北時區的今天
    pub date: String,
    pub results: Vec<IpUsage>,
}

/// 二、歷史一週用量查詢
#[derive(Debug, Serialize)]
pub struct WeekUsage {
    pub ip: String,
    /// 固定 7 筆，由舊到新（前六日 → 當日）
    pub days: Vec<DayUsage>,
}

#[derive(Debug, Serialize)]
pub struct DayUsage {
    pub day: String,
    pub bytes: i64,
}

/// 三、一日用量分佈
#[derive(Debug, Serialize)]
pub struct DailyDistribution {
    pub ip: String,
    pub date: String,
    pub bucket_seconds: i64,
    /// 一般日期固定 288 筆；查當日則只到目前所在的刻度為止
    pub points: Vec<BucketUsage>,
}

#[derive(Debug, Serialize)]
pub struct BucketUsage {
    /// 分桶起始時刻，RFC3339 帶 +08:00
    pub ts: String,
    pub bytes: i64,
}

/// 四、過量門檻檢查
#[derive(Debug, Serialize)]
pub struct ExceededReport {
    pub date: String,
    pub threshold_mib: f64,
    pub threshold_bytes: i64,
    pub count: usize,
    /// 依用量由大到小排序
    pub results: Vec<IpUsage>,
}
