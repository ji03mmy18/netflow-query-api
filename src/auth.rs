//! API Key 驗證與來源 IP 解析，以 axum extractor 形式提供。
//!
//! 做成 extractor 而不是 middleware 的理由：門檻檢查端點需要的條件
//! （Key + 來源 IP）比其他端點多一項，寫在 handler 簽章上比用兩層
//! middleware 分流更容易看出每支端點實際要求什麼。

use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::request::Parts;

use crate::error::ApiError;
use crate::state::AppState;

pub const API_KEY_HEADER: &str = "x-api-key";

/// 驗證通過的 API Key，內含設定檔裡的 name 供日誌辨識。
pub struct AuthenticatedKey(pub String);

impl FromRequestParts<AppState> for AuthenticatedKey {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let presented = parts
            .headers
            .get(API_KEY_HEADER)
            .and_then(|v| v.to_str().ok())
            .ok_or(ApiError::Unauthorized)?;

        // 逐一比對所有 key 且不提早 return：命中第幾把不該從回應時間看出來。
        let mut matched: Option<&str> = None;
        for entry in &state.config.config.auth.keys {
            if constant_time_eq(presented.as_bytes(), entry.key.as_bytes()) {
                matched = Some(&entry.name);
            }
        }

        match matched {
            Some(name) => Ok(Self(name.to_string())),
            None => Err(ApiError::Unauthorized),
        }
    }
}

/// 請求端的 IP。
///
/// 只有當 TCP 對端本身落在 `auth.trusted_proxies` 內時才採信
/// `X-Forwarded-For` / `X-Real-IP`；否則這兩個 header 是任何人都能
/// 自由填寫的欄位，採信它們等於讓白名單失效。
pub struct ClientIp(pub IpAddr);

impl FromRequestParts<AppState> for ClientIp {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(addr)| addr.ip())
            .ok_or_else(|| {
                // 只會在 server 未以 into_make_service_with_connect_info 啟動時發生。
                // 這種情況下無法判斷來源，一律拒絕而不是放行。
                tracing::error!("ConnectInfo missing; cannot determine client IP");
                ApiError::forbidden("伺服器無法判斷請求來源 IP")
            })?;

        if !state.config.is_trusted_proxy(peer) {
            return Ok(Self(peer));
        }

        Ok(Self(forwarded_for(parts, state).unwrap_or(peer)))
    }
}

/// 從右往左掃 X-Forwarded-For，取第一個不屬於受信任代理的位址。
///
/// 右端是最靠近本機的一跳。攻擊者能偽造的部分只有最左邊那幾個他自己
/// 塞進來的值，所以從右邊往左跳過所有已知代理之後停下的那一個，才是
/// 我們實際能驗證到的來源。
fn forwarded_for(parts: &Parts, state: &AppState) -> Option<IpAddr> {
    if let Some(xff) = parts.headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        let hops: Vec<IpAddr> = xff
            .split(',')
            .filter_map(|hop| hop.trim().parse::<IpAddr>().ok())
            .collect();

        if let Some(ip) = hops.iter().rev().find(|ip| !state.config.is_trusted_proxy(**ip)) {
            return Some(*ip);
        }
        // 整條鏈都是受信任的代理，取最左端。
        if let Some(first) = hops.first() {
            return Some(*first);
        }
    }

    parts
        .headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<IpAddr>().ok())
}

/// 長度不同直接判否（長度本身不是機密），內容以固定時間比對。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}
