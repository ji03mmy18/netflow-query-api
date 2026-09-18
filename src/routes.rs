//! 路由組裝與參數驗證。

use std::collections::HashSet;
use std::net::IpAddr;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use axum_extra::extract::Query;
use chrono::{Duration, NaiveDate, Utc};
use serde::Deserialize;

use crate::auth::{AuthenticatedKey, ClientIp};
use crate::error::{ApiError, ApiResult};
use crate::models::{DailyDistribution, ExceededReport, TodayUsage, WeekUsage};
use crate::query;
use crate::state::AppState;
use crate::timeutil::today_taipei;

const MIB: f64 = 1_048_576.0;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/api/v1/usage/today", get(today))
        .route("/api/v1/usage/week", get(week))
        .route("/api/v1/usage/daily", get(daily))
        .route("/api/v1/usage/exceeded", get(exceeded))
        .with_state(state)
}

/// 存活探測，刻意不驗 API Key，也刻意不碰資料庫：它回答的是「這個
/// process 還在嗎」，把 DB 狀態混進來會讓 orchestrator 在資料庫短暫
/// 抖動時重啟一個其實健康的服務。
async fn healthz() -> &'static str {
    "ok"
}

#[derive(Debug, Deserialize)]
struct TodayParams {
    #[serde(default)]
    ip: Vec<String>,
}

/// 一、當日用量查詢（單/多 IP，一次性全部回應）
async fn today(
    State(state): State<AppState>,
    _key: AuthenticatedKey,
    Query(params): Query<TodayParams>,
) -> ApiResult<Json<TodayUsage>> {
    let ips = parse_ips(&params.ip, state.config.config.limits.max_ips_per_request)?;
    let result = query::today_usage(&state.pool, today_taipei(), &ips).await?;
    Ok(Json(result))
}

#[derive(Debug, Deserialize)]
struct SingleIpParams {
    ip: String,
}

/// 二、歷史一週用量查詢（單 IP，當日 + 前六日）
async fn week(
    State(state): State<AppState>,
    _key: AuthenticatedKey,
    Query(params): Query<SingleIpParams>,
) -> ApiResult<Json<WeekUsage>> {
    let ip = parse_single_ip(&params.ip)?;
    let result = query::week_usage(&state.pool, today_taipei(), ip).await?;
    Ok(Json(result))
}

#[derive(Debug, Deserialize)]
struct DailyParams {
    ip: String,
    date: Option<String>,
}

/// 三、一日用量分佈（單 IP + 日期，5 分鐘刻度）
async fn daily(
    State(state): State<AppState>,
    _key: AuthenticatedKey,
    Query(params): Query<DailyParams>,
) -> ApiResult<Json<DailyDistribution>> {
    let ip = parse_single_ip(&params.ip)?;
    let today = today_taipei();

    let date = match params.date.as_deref() {
        Some(raw) => parse_date(raw)?,
        None => today,
    };

    if date > today {
        return Err(ApiError::validation(format!(
            "date {date} 是未來日期（今天是 {today}）"
        )));
    }

    // flow_stat_5m 有 retention policy，超過保留期的 chunk 已被刪除。
    // 若照常查下去會回一整片 0，看起來像「那天完全沒流量」而不是
    // 「資料已經不在了」——這種靜默的誤導比一個明確的錯誤糟得多。
    let max_age = state.config.config.limits.distribution_max_age_days;
    let earliest = today - Duration::days(max_age);
    if date < earliest {
        return Err(ApiError::validation(format!(
            "date {date} 超出 5 分鐘統計的保留範圍（最早 {earliest}）；\
             該區間的原始分桶已被 retention policy 清除，僅日統計仍保留"
        )));
    }

    let result = query::daily_distribution(&state.pool, date, ip, Utc::now()).await?;
    Ok(Json(result))
}

#[derive(Debug, Deserialize)]
struct ExceededParams {
    /// 回應的 key 已是 camelCase（`thresholdMib`），查詢字串兩種寫法都收：
    /// 只支援一種，必然會有人照著回應的欄位名去拼參數然後拿到 422。
    #[serde(alias = "thresholdMib")]
    threshold_mib: f64,
}

/// 四、過量門檻檢查
///
/// 這支端點會列舉所有超標的 IP，等於間接吐出監控清單，因此除了 API Key
/// 之外額外要求來源 IP 落在 `[threshold_check].allowed_ips`。白名單為空
/// 時端點不啟用（fail-closed）——漏設定的後果是查不到，而不是全開。
async fn exceeded(
    State(state): State<AppState>,
    key: AuthenticatedKey,
    ClientIp(client_ip): ClientIp,
    Query(params): Query<ExceededParams>,
) -> ApiResult<Json<ExceededReport>> {
    if !state.config.threshold_check_enabled() {
        tracing::warn!(
            key = %key.0,
            %client_ip,
            "threshold check requested but threshold_check.allowed_ips is empty"
        );
        return Err(ApiError::forbidden(
            "門檻檢查端點未啟用：threshold_check.allowed_ips 為空",
        ));
    }

    if !state.config.threshold_check_allows(client_ip) {
        tracing::warn!(key = %key.0, %client_ip, "threshold check rejected: source IP not allowed");
        return Err(ApiError::forbidden(format!(
            "來源 IP {client_ip} 不在門檻檢查的允許名單內"
        )));
    }

    let threshold_mib = params.threshold_mib;
    if !threshold_mib.is_finite() || threshold_mib < 0.0 {
        return Err(ApiError::validation(
            "threshold_mib 必須是 0 或正的有限數值",
        ));
    }

    // MiB 轉 bytes 後可能超出 i64（門檻寫成天文數字時），夾在上限比讓
    // `as i64` 做飽和轉換更明確。
    let threshold_bytes = (threshold_mib * MIB).round();
    if threshold_bytes > i64::MAX as f64 {
        return Err(ApiError::validation("threshold_mib 過大"));
    }

    tracing::info!(key = %key.0, %client_ip, threshold_mib, "threshold check");

    let result =
        query::exceeded(&state.pool, today_taipei(), threshold_mib, threshold_bytes as i64).await?;
    Ok(Json(result))
}

/// 解析 IP 清單。
///
/// 同時接受 `?ip=a&ip=b` 與 `?ip=a,b`：兩種寫法在不同 HTTP 客戶端裡都很
/// 常見，只支援一種必然會有人踩到。
fn parse_ips(raw: &[String], max: usize) -> ApiResult<Vec<IpAddr>> {
    let mut seen = HashSet::new();
    let mut ips = Vec::new();

    for entry in raw.iter().flat_map(|v| v.split(',')) {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let ip: IpAddr = entry
            .parse()
            .map_err(|_| ApiError::validation(format!("\"{entry}\" 不是合法的 IP 位址")))?;

        // 重複的 IP 只查一次：否則回應裡會出現同一個位址的多筆結果。
        if seen.insert(ip) {
            ips.push(ip);
        }
    }

    if ips.is_empty() {
        return Err(ApiError::validation("至少需要一個 ip 參數"));
    }
    if ips.len() > max {
        return Err(ApiError::validation(format!(
            "單次最多查詢 {max} 個 IP，收到 {}",
            ips.len()
        )));
    }

    Ok(ips)
}

fn parse_single_ip(raw: &str) -> ApiResult<IpAddr> {
    raw.trim()
        .parse()
        .map_err(|_| ApiError::validation(format!("\"{raw}\" 不是合法的 IP 位址")))
}

fn parse_date(raw: &str) -> ApiResult<NaiveDate> {
    NaiveDate::parse_from_str(raw.trim(), "%Y-%m-%d")
        .map_err(|_| ApiError::validation(format!("date \"{raw}\" 必須是 YYYY-MM-DD 格式")))
}
