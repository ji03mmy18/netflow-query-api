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
use crate::models::{DailyDistribution, ExceededReport, TodayUsage, TopUsage, WeekUsage};
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
        .route("/api/v1/usage/top", get(top))
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
    let date = resolve_date(params.date.as_deref(), today)?;

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

    /// 選填，`YYYY-MM-DD`（台北時區）。省略時查當日。
    date: Option<String>,
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
    require_allowed_source(
        state.config.threshold_check_enabled(),
        state.config.threshold_check_allows(client_ip),
        client_ip,
        &key.0,
        "threshold_check",
    )?;

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

    // flow_stat_1d 沒有 retention policy（schema 明言長期保留），所以這裡
    // 只擋未來日期，不像 daily 還要擋過舊的日期。
    let date = resolve_date(params.date.as_deref(), today_taipei())?;

    tracing::info!(key = %key.0, %client_ip, threshold_mib, %date, "threshold check");

    let result = query::exceeded(&state.pool, date, threshold_mib, threshold_bytes as i64).await?;
    Ok(Json(result))
}

#[derive(Debug, Deserialize)]
struct TopParams {
    /// 選填，回傳筆數。省略時用 `limits.top_n_default_limit`。
    limit: Option<usize>,

    /// 選填，`YYYY-MM-DD`（台北時區）。省略時查當日。
    date: Option<String>,
}

/// 五、Top-N 用量排行
///
/// 與 exceeded 一樣會列舉 IP，而且呼叫端連門檻都不必猜就能拿到流量最高的
/// 主機清單，所以同樣要求來源 IP 落在白名單——只是用獨立的
/// `[top_n].allowed_ips`，兩支端點可以分別開關。
async fn top(
    State(state): State<AppState>,
    key: AuthenticatedKey,
    ClientIp(client_ip): ClientIp,
    Query(params): Query<TopParams>,
) -> ApiResult<Json<TopUsage>> {
    require_allowed_source(
        state.config.top_n_enabled(),
        state.config.top_n_allows(client_ip),
        client_ip,
        &key.0,
        "top_n",
    )?;

    let limits = &state.config.config.limits;
    let limit = params.limit.unwrap_or(limits.top_n_default_limit);

    // 超出範圍回 422 而不是夾到邊界：靜默把 limit=10000 改成 500，呼叫端
    // 會以為自己拿到了完整的前一萬名。
    if limit == 0 || limit > limits.top_n_max_limit {
        return Err(ApiError::validation(format!(
            "limit must be between 1 and {} (received {limit})",
            limits.top_n_max_limit
        )));
    }

    let date = resolve_date(params.date.as_deref(), today_taipei())?;

    tracing::info!(key = %key.0, %client_ip, limit, %date, "top-n query");

    let result = query::top_usage(&state.pool, date, limit).await?;
    Ok(Json(result))
}

/// 會列舉 IP 位址的端點共用的來源檢查。
///
/// 白名單為空時視為「該端點未啟用」而非「不限制來源」——漏設定的後果
/// 應該是查不到，而不是全開。
fn require_allowed_source(
    enabled: bool,
    allowed: bool,
    client_ip: std::net::IpAddr,
    key_name: &str,
    section: &str,
) -> ApiResult<()> {
    if !enabled {
        tracing::warn!(
            key = %key_name,
            %client_ip,
            section,
            "request rejected: allow list is empty, endpoint is disabled"
        );
        return Err(ApiError::forbidden(format!(
            "this endpoint is disabled: {section}.allowed_ips is empty"
        )));
    }

    if !allowed {
        tracing::warn!(
            key = %key_name,
            %client_ip,
            section,
            "request rejected: source IP not in allow list"
        );
        return Err(ApiError::forbidden(format!(
            "source IP {client_ip} is not in the {section} allow list"
        )));
    }

    Ok(())
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

/// 解析選填的 `date` 參數，省略時用今天；不接受未來日期。
///
/// daily 與 exceeded 共用：未來日期的判斷若在兩處各寫一次，日後只改一邊
/// 就會出現「這支端點接受明天、那支不接受」這種說不出道理的差異。
fn resolve_date(raw: Option<&str>, today: NaiveDate) -> ApiResult<NaiveDate> {
    let date = match raw {
        Some(raw) => parse_date(raw)?,
        None => today,
    };

    if date > today {
        return Err(ApiError::validation(format!(
            "date {date} is in the future (today is {today})"
        )));
    }

    Ok(date)
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

    /// 與 CONFIG 相同，但啟用了門檻檢查，來源允許 127.0.0.1。
    /// 用來測試門檻端點在通過授權之後的參數驗證。
    const CONFIG_WITH_THRESHOLD: &str = r#"
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

[threshold_check]
allowed_ips = ["127.0.0.1"]
"#;

    /// 與 CONFIG 相同，但只啟用 Top-N（threshold_check 仍為空）。
    /// 用來驗證兩支端點的白名單互相獨立。
    const CONFIG_WITH_TOP_N: &str = r#"
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

[top_n]
allowed_ips = ["127.0.0.1"]
"#;

    /// 組出真正的 router，但 pool 用 `connect_lazy_with`——它不會建立任何
    /// 連線，所以不需要資料庫就能測試整條中介層堆疊。
    ///
    /// 條件是測試請求不能走到查詢：認證失敗、參數驗證失敗、門檻端點的
    /// 來源 IP 檢查、healthz 都在碰資料庫之前就回應了。
    fn app() -> Router {
        app_with(CONFIG)
    }

    fn app_with(toml: &str) -> Router {
        let config = LoadedConfig::from_toml_str(toml).expect("test config should be valid");
        // acquire_timeout 設得極短：這些測試預期都不該走到查詢，萬一有測試
        // 意外碰到資料庫，要立刻失敗而不是卡滿預設的 30 秒逾時。
        let pool = PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(50))
            .connect_lazy_with(PgConnectOptions::new());
        router(AppState {
            pool,
            config: Arc::new(config),
        })
    }

    /// 會列舉 IP 的端點需要 ConnectInfo 才能判斷來源；沒有它會一律拒絕。
    fn signed_request(uri: &str) -> Request<Body> {
        let mut req = request(uri)
            .header(crate::auth::API_KEY_HEADER, API_KEY)
            .body(Body::empty())
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 40000))));
        req
    }

    fn exceeded_request(query: &str) -> Request<Body> {
        signed_request(&format!("/api/v1/usage/exceeded?{query}"))
    }

    fn top_request(query: &str) -> Request<Body> {
        let uri = if query.is_empty() {
            "/api/v1/usage/top".to_string()
        } else {
            format!("/api/v1/usage/top?{query}")
        };
        signed_request(&uri)
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
        let response = app()
            .oneshot(exceeded_request("threshold_mib=1"))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    /// date 是選填的；省略時查當日。這裡只驗證參數驗證有放行——
    /// 實際查詢需要資料庫，不在這個測試的範圍。
    #[tokio::test]
    async fn exceeded_rejects_future_date() {
        let response = app_with(CONFIG_WITH_THRESHOLD)
            .oneshot(exceeded_request("threshold_mib=1&date=2099-01-01"))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn exceeded_rejects_malformed_date() {
        let response = app_with(CONFIG_WITH_THRESHOLD)
            .oneshot(exceeded_request("threshold_mib=1&date=2026%2F09%2F01"))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    /// 釘住 `parse_date` 的實際行為。
    ///
    /// chrono 的 `%m` / `%d` 接受未補零的數字，所以 `2026-9-1` 是合法輸入
    /// ——這跟錯誤訊息裡寫的 "YYYY-MM-DD" 讀起來不太一樣，寫下來免得日後
    /// 有人照字面假設它是嚴格的。
    #[test]
    fn parse_date_behaviour() {
        assert!(parse_date("2026-09-01").is_ok());
        assert!(parse_date("2026-9-1").is_ok(), "未補零的數字是被接受的");

        assert!(parse_date("2026/09/01").is_err(), "分隔符必須是 -");
        assert!(parse_date("20260901").is_err(), "不能省略分隔符");
        assert!(parse_date("2026-13-01").is_err(), "月份超出範圍");
        assert!(parse_date("2026-02-30").is_err(), "日期不存在");
        assert!(parse_date("2026-09-01T00:00:00Z").is_err(), "不接受時間部分");
        assert!(parse_date("").is_err());
    }

    /// daily 與 exceeded 共用 resolve_date，未來日期的行為必須一致。
    #[tokio::test]
    async fn daily_rejects_future_date() {
        let response = app()
            .oneshot(
                request("/api/v1/usage/daily?ip=10.1.2.3&date=2099-01-01")
                    .header(crate::auth::API_KEY_HEADER, API_KEY)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
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

    #[tokio::test]
    async fn top_is_closed_when_allow_list_is_empty() {
        let response = app().oneshot(top_request("")).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    /// threshold_check 有設但 top_n 沒設時，Top-N 仍然關閉。
    /// 兩份白名單是獨立的，不該互相解鎖。
    #[tokio::test]
    async fn top_is_not_unlocked_by_the_threshold_allow_list() {
        let response = app_with(CONFIG_WITH_THRESHOLD)
            .oneshot(top_request(""))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    /// 反向：top_n 有設但 threshold_check 沒設時，門檻端點仍然關閉。
    #[tokio::test]
    async fn exceeded_is_not_unlocked_by_the_top_n_allow_list() {
        let response = app_with(CONFIG_WITH_TOP_N)
            .oneshot(exceeded_request("threshold_mib=1"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn top_rejects_zero_limit() {
        let response = app_with(CONFIG_WITH_TOP_N)
            .oneshot(top_request("limit=0"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    /// 超過上限回 422 而不是夾到 500——靜默截斷會讓呼叫端以為
    /// 自己拿到了完整的排行。
    #[tokio::test]
    async fn top_rejects_limit_above_maximum() {
        let response = app_with(CONFIG_WITH_TOP_N)
            .oneshot(top_request("limit=501"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn top_rejects_future_date() {
        let response = app_with(CONFIG_WITH_TOP_N)
            .oneshot(top_request("date=2099-01-01"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn top_requires_an_api_key() {
        use axum::body::Body;
        use axum::extract::ConnectInfo;
        use axum::http::Request;
        use std::net::SocketAddr;

        let mut req = Request::builder()
            .uri("/api/v1/usage/top")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 40000))));

        let response = app_with(CONFIG_WITH_TOP_N).oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
