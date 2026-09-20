//! 路由組裝與參數驗證。

use std::collections::HashSet;
use std::net::IpAddr;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use axum_extra::extract::Query;
use chrono::{Duration, NaiveDate, Utc};
use serde::Deserialize;
use tower_http::compression::CompressionLayer;

use crate::auth::{AuthenticatedKey, ClientIp};
use crate::error::{ApiError, ApiResult};
use crate::models::{DailyDistribution, ExceededReport, TodayUsage, WeekUsage};
use crate::query;
use crate::state::AppState;
use crate::timeutil::today_taipei;

const MIB: f64 = 1_048_576.0;

/// 組裝路由與應用層中介層。
///
/// CompressionLayer 放在這裡而不是 main：測試若要自己重建一次中介層堆疊，
/// 就會變成「測試通過但正式啟動的設定不同」——壓縮相關的行為必須以同一
/// 個組裝結果為準。TraceLayer 留在 main，它是觀測設定而非應用行為，且要
/// 在最外層才能記錄到實際送出的回應。
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/api/v1/usage/today", get(today))
        .route("/api/v1/usage/week", get(week))
        .route("/api/v1/usage/daily", get(daily))
        .route("/api/v1/usage/exceeded", get(exceeded))
        .with_state(state)
        // 只在請求帶 Accept-Encoding: gzip 時才壓縮；沒帶就原樣回傳，
        // 因此不會影響任何既有呼叫端。預設跳過小於 32 bytes 的回應——
        // 壓縮那種大小的內容只會變大。
        .layer(CompressionLayer::new())
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
            "date {date} is in the future (today is {today})"
        )));
    }

    // flow_stat_5m 有 retention policy，超過保留期的 chunk 已被刪除。
    // 若照常查下去會回一整片 0，看起來像「那天完全沒流量」而不是
    // 「資料已經不在了」——這種靜默的誤導比一個明確的錯誤糟得多。
    let max_age = state.config.config.limits.distribution_max_age_days;
    let earliest = today - Duration::days(max_age);
    if date < earliest {
        return Err(ApiError::validation(format!(
            "date {date} is outside the retention window of the 5-minute statistics \
             (earliest available: {earliest}); those buckets have been dropped by the \
             retention policy, only daily totals remain"
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
            "threshold check is disabled: threshold_check.allowed_ips is empty",
        ));
    }

    if !state.config.threshold_check_allows(client_ip) {
        tracing::warn!(key = %key.0, %client_ip, "threshold check rejected: source IP not allowed");
        return Err(ApiError::forbidden(format!(
            "source IP {client_ip} is not in the threshold check allow list"
        )));
    }

    let threshold_mib = params.threshold_mib;
    if !threshold_mib.is_finite() || threshold_mib < 0.0 {
        return Err(ApiError::validation(
            "threshold_mib must be a finite number greater than or equal to 0",
        ));
    }

    // MiB 轉 bytes 後可能超出 i64（門檻寫成天文數字時），夾在上限比讓
    // `as i64` 做飽和轉換更明確。
    let threshold_bytes = (threshold_mib * MIB).round();
    if threshold_bytes > i64::MAX as f64 {
        return Err(ApiError::validation("threshold_mib is too large"));
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
            .map_err(|_| ApiError::validation(format!("\"{entry}\" is not a valid IP address")))?;

        // 重複的 IP 只查一次：否則回應裡會出現同一個位址的多筆結果。
        if seen.insert(ip) {
            ips.push(ip);
        }
    }

    if ips.is_empty() {
        return Err(ApiError::validation("at least one ip parameter is required"));
    }
    if ips.len() > max {
        return Err(ApiError::validation(format!(
            "at most {max} IP addresses per request, received {}",
            ips.len()
        )));
    }

    Ok(ips)
}

fn parse_single_ip(raw: &str) -> ApiResult<IpAddr> {
    raw.trim()
        .parse()
        .map_err(|_| ApiError::validation(format!("\"{raw}\" is not a valid IP address")))
}

fn parse_date(raw: &str) -> ApiResult<NaiveDate> {
    NaiveDate::parse_from_str(raw.trim(), "%Y-%m-%d")
        .map_err(|_| ApiError::validation(format!("date \"{raw}\" must be in YYYY-MM-DD format")))
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::net::SocketAddr;
    use std::sync::Arc;

    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::{Request, StatusCode, header};
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
    use tower::ServiceExt;

    use super::*;
    use crate::config::LoadedConfig;

    const API_KEY: &str = "test-key";

    const CONFIG: &str = r#"
[server]
host = "127.0.0.1"
port = 8081

[database]
host = "127.0.0.1"
port = 5432
name = "netflow"
user = "netflow_ro"
password = "x"

[[auth.keys]]
name = "dashboard"
key = "test-key"
"#;

    /// 組出真正的 router，但 pool 用 `connect_lazy_with`——它不會建立任何
    /// 連線，所以不需要資料庫就能測試整條中介層堆疊。
    ///
    /// 條件是測試請求不能走到查詢：認證失敗、參數驗證失敗、門檻端點的
    /// 來源 IP 檢查、healthz 都在碰資料庫之前就回應了。
    fn app() -> Router {
        let config = LoadedConfig::from_toml_str(CONFIG).expect("test config should be valid");
        let pool = PgPoolOptions::new().connect_lazy_with(PgConnectOptions::new());
        router(AppState {
            pool,
            config: Arc::new(config),
        })
    }

    fn request(uri: &str) -> axum::http::request::Builder {
        Request::builder().uri(uri)
    }

    async fn body_bytes(response: axum::response::Response) -> Vec<u8> {
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body should be readable")
            .to_vec()
    }

    #[tokio::test]
    async fn healthz_needs_no_api_key() {
        let response = app()
            .oneshot(request("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn missing_api_key_is_rejected() {
        let response = app()
            .oneshot(
                request("/api/v1/usage/today?ip=10.1.2.3")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn wrong_api_key_is_rejected() {
        let response = app()
            .oneshot(
                request("/api/v1/usage/today?ip=10.1.2.3")
                    .header(crate::auth::API_KEY_HEADER, "not-the-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn invalid_ip_is_rejected_before_touching_the_database() {
        let response = app()
            .oneshot(
                request("/api/v1/usage/today?ip=not-an-ip")
                    .header(crate::auth::API_KEY_HEADER, API_KEY)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    /// 門檻端點在 `threshold_check.allowed_ips` 為空時必須一律拒絕
    /// （fail-closed）。漏設定的後果應該是查不到，而不是全開。
    #[tokio::test]
    async fn threshold_endpoint_is_closed_when_allow_list_is_empty() {
        let mut req = request("/api/v1/usage/exceeded?threshold_mib=1")
            .header(crate::auth::API_KEY_HEADER, API_KEY)
            .body(Body::empty())
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 40000))));

        let response = app().oneshot(req).await.unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    /// 帶 Accept-Encoding: gzip 時回應要被壓縮，且解開後與未壓縮版本相同。
    #[tokio::test]
    async fn gzip_is_applied_when_requested() {
        let uri = "/api/v1/usage/today?ip=not-an-ip";

        let plain = app()
            .oneshot(
                request(uri)
                    .header(crate::auth::API_KEY_HEADER, API_KEY)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let plain_body = body_bytes(plain).await;

        let compressed = app()
            .oneshot(
                request(uri)
                    .header(crate::auth::API_KEY_HEADER, API_KEY)
                    .header(header::ACCEPT_ENCODING, "gzip")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            compressed
                .headers()
                .get(header::CONTENT_ENCODING)
                .map(|v| v.to_str().unwrap()),
            Some("gzip"),
            "帶 Accept-Encoding: gzip 卻沒有壓縮"
        );

        let raw = body_bytes(compressed).await;
        assert_ne!(raw, plain_body, "標示為 gzip 但內容未經壓縮");

        let mut decoded = Vec::new();
        flate2::read::GzDecoder::new(&raw[..])
            .read_to_end(&mut decoded)
            .expect("gzip body should decode");

        assert_eq!(decoded, plain_body, "解壓後的內容與未壓縮版本不一致");
    }

    /// 沒有 Accept-Encoding 時必須原樣回傳。
    /// 這是「不支援壓縮的呼叫端照樣能用」的保證。
    #[tokio::test]
    async fn no_compression_without_accept_encoding() {
        let response = app()
            .oneshot(
                request("/api/v1/usage/today?ip=not-an-ip")
                    .header(crate::auth::API_KEY_HEADER, API_KEY)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert!(
            response.headers().get(header::CONTENT_ENCODING).is_none(),
            "未要求壓縮卻回了 content-encoding"
        );

        let body = body_bytes(response).await;
        let text = String::from_utf8(body).expect("body should be UTF-8");
        assert!(
            text.starts_with('{') && text.contains("VALIDATION_ERROR"),
            "未壓縮的回應應該是可直接閱讀的 JSON，實際為：{text}"
        );
    }
}
