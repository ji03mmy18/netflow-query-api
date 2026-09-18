//! 台北時區的日界線計算。
//!
//! 「當日」一律在 Rust 端算完再綁進 SQL，不使用 `CURRENT_DATE`。
//!
//! 原因在 schema 裡：那段 `ALTER DATABASE ... SET timezone` 在連線帳號
//! 不是資料庫擁有者時只會發一個 NOTICE 就跳過，不報錯。一旦沒套用成
//! 功，`CURRENT_DATE` 會退回 UTC，台灣時間凌晨 0 點到 8 點之間就會查到
//! 前一天的資料——而且完全靜默。把日期算在應用層，這個失效模式就不存在。

use chrono::{DateTime, Duration, NaiveDate, NaiveTime, TimeZone, Utc};
use chrono_tz::Asia::Taipei;

/// `flow_stat_5m` 的分桶長度（秒）。
pub const BUCKET_SECONDS: i64 = 300;

/// 一天的 5 分鐘刻度數。台北沒有日光節約時間，這個數字恆為 288。
pub const BUCKETS_PER_DAY: usize = 288;

pub fn today_taipei() -> NaiveDate {
    Utc::now().with_timezone(&Taipei).date_naive()
}

/// 回傳該台北日期的 `[起, 迄)` 絕對時刻，用於比對 `timestamptz`。
///
/// 台北自 1979 年起沒有日光節約時間，午夜不會有不存在或重複的情況；
/// 仍然用 `single()` 明確處理，避免依賴這個前提而不自知。
pub fn day_bounds_utc(date: NaiveDate) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let start = Taipei
        .from_local_datetime(&date.and_time(NaiveTime::MIN))
        .single()?
        .with_timezone(&Utc);
    let end = start + Duration::days(1);
    Some((start, end))
}

/// 以 `+08:00` 偏移輸出 RFC3339，讓呼叫端不必自己換算時區。
pub fn to_taipei_rfc3339(ts: DateTime<Utc>) -> String {
    ts.with_timezone(&Taipei).to_rfc3339()
}
