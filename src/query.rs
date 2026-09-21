//! 四支查詢的 SQL 與補零邏輯。
//!
//! 四個 bytes 欄位原樣取出，對外合計（`internetTotalBytes`）在 Rust 端算，
//! 定義集中在 [`Usage::new`] 一處。
//!
//! 補零一律在 Rust 端做，不使用 `time_bucket_gapfill`：
//!   - `flow_stat_1d` 是普通表、`day` 是 `date`，gapfill 用起來不順；
//!   - 多 IP 查詢時，整個區間都沒資料的 IP 根本不會進 gapfill 的輸出，
//!     還是得在應用層補一筆空的，兩邊各做一半反而更難讀。
//! 點數本來就有上限（7 筆 / 288 筆），在 Rust 補的成本可以忽略。

use std::collections::HashMap;
use std::net::IpAddr;

use chrono::{DateTime, Duration, NaiveDate, Utc};
use sqlx::PgPool;

use crate::error::ApiResult;
use crate::models::{
    BucketUsage, DailyDistribution, DayUsage, ExceededEntry, ExceededReport, IpUsage, TodayUsage,
    TopUsage, Usage, WeekUsage,
};
use crate::timeutil::{BUCKETS_PER_DAY, BUCKET_SECONDS, day_bounds_utc, to_taipei_rfc3339};

/// 四個 bytes 欄位的取用順序，所有查詢共用，避免各處自行拼寫而錯位。
const BYTE_COLUMNS: &str = "ext_rx_bytes, ext_tx_bytes, intra_rx_bytes, intra_tx_bytes";

/// 一、當日用量（多 IP）
///
/// `ips` 的順序與內容原樣保留在回應中：沒有資料的 IP 補 0，呼叫端送出的
/// 每一個位址都會拿到一筆結果，不必自己比對哪些被省略了。
pub async fn today_usage(pool: &PgPool, day: NaiveDate, ips: &[IpAddr]) -> ApiResult<TodayUsage> {
    let sql = format!(
        "SELECT host(addr) AS ip, {BYTE_COLUMNS}
         FROM flow_stat_1d
         WHERE day = $1
           AND addr = ANY($2)"
    );

    let rows: Vec<(String, i64, i64, i64, i64)> = sqlx::query_as(&sql)
        .bind(day)
        .bind(ips)
        .fetch_all(pool)
        .await?;

    // 以 IpAddr 而非字串當 key：host(addr) 的正規化形式由 Postgres 決定，
    // IpAddr::to_string() 的由 Rust 決定。IPv4 兩者必然一致，IPv6 則不保證
    // （壓縮零段的寫法、IPv4-mapped 的呈現方式都有空間）。一旦不一致，
    // 症狀是「這個 IP 明明有資料卻回報 0」，沒有任何錯誤訊息。
    let mut found: HashMap<IpAddr, Usage> = rows
        .into_iter()
        .filter_map(|(text, ext_rx, ext_tx, intra_rx, intra_tx)| {
            match text.parse::<IpAddr>() {
                Ok(ip) => Some((ip, Usage::new(ext_rx, ext_tx, intra_rx, intra_tx))),
                Err(_) => {
                    tracing::error!(addr = %text, "host(addr) returned an unparsable address");
                    None
                }
            }
        })
        .collect();

    let results = ips
        .iter()
        .map(|ip| IpUsage {
            ip: ip.to_string(),
            usage: found.remove(ip).unwrap_or_else(Usage::zero),
        })
        .collect();

    Ok(TodayUsage {
        date: day.to_string(),
        results,
    })
}

/// 二、歷史一週用量（單 IP）：當日 + 前六日，固定 7 筆
pub async fn week_usage(pool: &PgPool, today: NaiveDate, ip: IpAddr) -> ApiResult<WeekUsage> {
    let first = today - Duration::days(6);

    let sql = format!(
        "SELECT day, {BYTE_COLUMNS}
         FROM flow_stat_1d
         WHERE addr = $1
           AND day >= $2
           AND day <= $3
         ORDER BY day"
    );

    let rows: Vec<(NaiveDate, i64, i64, i64, i64)> = sqlx::query_as(&sql)
        .bind(ip)
        .bind(first)
        .bind(today)
        .fetch_all(pool)
        .await?;

    let mut found: HashMap<NaiveDate, Usage> = rows
        .into_iter()
        .map(|(day, ext_rx, ext_tx, intra_rx, intra_tx)| {
            (day, Usage::new(ext_rx, ext_tx, intra_rx, intra_tx))
        })
        .collect();

    let days = (0..7)
        .map(|offset| {
            let day = first + Duration::days(offset);
            DayUsage {
                day: day.to_string(),
                usage: found.remove(&day).unwrap_or_else(Usage::zero),
            }
        })
        .collect();

    Ok(WeekUsage {
        ip: ip.to_string(),
        days,
    })
}

/// 三、一日用量分佈（單 IP + 日期），5 分鐘刻度
///
/// `now` 由呼叫端傳入而非在這裡取：查詢當日時要知道「到目前為止」的界線
/// 在哪，把時鐘當參數傳進來，這段補零邏輯才能被測試。
pub async fn daily_distribution(
    pool: &PgPool,
    date: NaiveDate,
    ip: IpAddr,
    now: DateTime<Utc>,
) -> ApiResult<DailyDistribution> {
    let (start, end) = day_bounds_utc(date)
        .ok_or_else(|| crate::error::ApiError::validation(format!("{date} is not a valid date")))?;

    let sql = format!(
        "SELECT bucket, {BYTE_COLUMNS}
         FROM flow_stat_5m
         WHERE addr = $1
           AND bucket >= $2
           AND bucket <  $3
         ORDER BY bucket"
    );

    let rows: Vec<(DateTime<Utc>, i64, i64, i64, i64)> = sqlx::query_as(&sql)
        .bind(ip)
        .bind(start)
        .bind(end)
        .fetch_all(pool)
        .await?;

    let mut found: HashMap<DateTime<Utc>, Usage> = rows
        .into_iter()
        .map(|(bucket, ext_rx, ext_tx, intra_rx, intra_tx)| {
            (bucket, Usage::new(ext_rx, ext_tx, intra_rx, intra_tx))
        })
        .collect();

    // 查當日時只補到現在所在的刻度：把尚未到來的時間也補成 0，圖表尾端
    // 會出現一段無法分辨「還沒發生」與「真的沒流量」的平坦區。
    let mut points = Vec::with_capacity(BUCKETS_PER_DAY);
    for index in 0..BUCKETS_PER_DAY as i64 {
        let ts = start + Duration::seconds(index * BUCKET_SECONDS);
        if ts > now {
            break;
        }
        points.push(BucketUsage {
            ts: to_taipei_rfc3339(ts),
            usage: found.remove(&ts).unwrap_or_else(Usage::zero),
        });
    }

    Ok(DailyDistribution {
        ip: ip.to_string(),
        date: date.to_string(),
        bucket_seconds: BUCKET_SECONDS,
        points,
    })
}

/// 四、過量門檻檢查：當日「對外合計」超過門檻的所有 IP
///
/// 門檻固定比對 `ext_rx_bytes + ext_tx_bytes`，不含校內流量。
pub async fn exceeded(
    pool: &PgPool,
    day: NaiveDate,
    threshold_mib: f64,
    threshold_bytes: i64,
) -> ApiResult<ExceededReport> {
    // 排序與篩選都寫成完整運算式，不用輸出欄位的序數（ORDER BY 2）：
    // 欄位一增減，序數指到的東西就變了，而這種錯不會有任何錯誤訊息。
    let sql = format!(
        "SELECT host(addr) AS ip, {BYTE_COLUMNS}
         FROM flow_stat_1d
         WHERE day = $1
           AND ext_rx_bytes + ext_tx_bytes > $2
         ORDER BY ext_rx_bytes + ext_tx_bytes DESC, addr ASC"
    );

    let rows: Vec<(String, i64, i64, i64, i64)> = sqlx::query_as(&sql)
        .bind(day)
        .bind(threshold_bytes)
        .fetch_all(pool)
        .await?;

    // 正規化成 Rust 的形式，讓這支端點吐出的 IP 字串能直接餵回
    // today 端點的 ?ip= 參數，不必擔心兩邊的寫法對不上。
    let results: Vec<ExceededEntry> = rows
        .into_iter()
        .filter_map(|(text, ext_rx, ext_tx, intra_rx, intra_tx)| {
            match text.parse::<IpAddr>() {
                Ok(ip) => Some(ExceededEntry::new(
                    ip.to_string(),
                    Usage::new(ext_rx, ext_tx, intra_rx, intra_tx),
                    threshold_bytes,
                )),
                Err(_) => {
                    tracing::error!(addr = %text, "host(addr) returned an unparsable address");
                    None
                }
            }
        })
        .collect();

    Ok(ExceededReport {
        date: day.to_string(),
        threshold_mib,
        threshold_bytes,
        count: results.len(),
        results,
    })
}

/// 五、Top-N：當日對外用量最高的 N 台主機
///
/// 排除對外用量為 0 的主機：只有校內流量的機器出現在「流量排行」上只是
/// 雜訊，而且當日有資料的主機不足 N 台時，補幾筆 0 進來並不會讓結果更
/// 有用。因此回傳筆數可能少於 `limit`。
pub async fn top_usage(pool: &PgPool, day: NaiveDate, limit: usize) -> ApiResult<TopUsage> {
    let sql = format!(
        "SELECT host(addr) AS ip, {BYTE_COLUMNS}
         FROM flow_stat_1d
         WHERE day = $1
           AND ext_rx_bytes + ext_tx_bytes > 0
         ORDER BY ext_rx_bytes + ext_tx_bytes DESC, addr ASC
         LIMIT $2"
    );

    // limit 已在 handler 端被夾在 1..=top_n_max_limit，轉 i64 不會溢位。
    let rows: Vec<(String, i64, i64, i64, i64)> = sqlx::query_as(&sql)
        .bind(day)
        .bind(limit as i64)
        .fetch_all(pool)
        .await?;

    let results: Vec<IpUsage> = rows
        .into_iter()
        .filter_map(|(text, ext_rx, ext_tx, intra_rx, intra_tx)| {
            match text.parse::<IpAddr>() {
                Ok(ip) => Some(IpUsage {
                    ip: ip.to_string(),
                    usage: Usage::new(ext_rx, ext_tx, intra_rx, intra_tx),
                }),
                Err(_) => {
                    tracing::error!(addr = %text, "host(addr) returned an unparsable address");
                    None
                }
            }
        })
        .collect();

    Ok(TopUsage {
        date: day.to_string(),
        limit,
        count: results.len(),
        results,
    })
}
