//! 設定檔載入與正規化。
//!
//! 設定一律來自 TOML 檔，不讀環境變數——這個服務只有一個部署形態，
//! 兩套來源只會讓「現在生效的是哪個值」變得難以回答。

use std::net::IpAddr;
use std::path::Path;

use ipnetwork::IpNetwork;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    pub auth: AuthConfig,
    #[serde(default)]
    pub threshold_check: AllowListConfig,
    #[serde(default)]
    pub top_n: AllowListConfig,
    #[serde(default)]
    pub limits: LimitsConfig,
}

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Deserialize)]
pub struct DatabaseConfig {
    pub host: String,
    pub port: u16,
    pub name: String,
    pub user: String,
    pub password: String,
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
}

fn default_max_connections() -> u32 {
    5
}

#[derive(Debug, Deserialize)]
pub struct AuthConfig {
    pub keys: Vec<ApiKeyConfig>,
    /// 原始字串形式；正規化後的網段見 [`Config::trusted_proxies`]。
    #[serde(default)]
    pub trusted_proxies: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct ApiKeyConfig {
    pub name: String,
    pub key: String,
}

/// 來源 IP 白名單。
///
/// `[threshold_check]` 與 `[top_n]` 形狀相同——兩者都是「會列舉 IP 位址的
/// 端點」，都需要在 API Key 之外再驗來源。共用同一個型別，日後加欄位
/// （例如速率限制）兩邊會一起得到。
#[derive(Debug, Default, Deserialize)]
pub struct AllowListConfig {
    #[serde(default)]
    pub allowed_ips: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct LimitsConfig {
    #[serde(default = "default_max_ips")]
    pub max_ips_per_request: usize,
    #[serde(default = "default_max_age_days")]
    pub distribution_max_age_days: i64,
    #[serde(default = "default_top_n_limit")]
    pub top_n_default_limit: usize,
    #[serde(default = "default_top_n_max_limit")]
    pub top_n_max_limit: usize,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_ips_per_request: default_max_ips(),
            distribution_max_age_days: default_max_age_days(),
            top_n_default_limit: default_top_n_limit(),
            top_n_max_limit: default_top_n_max_limit(),
        }
    }
}

fn default_max_ips() -> usize {
    50
}

fn default_max_age_days() -> i64 {
    395
}

fn default_top_n_limit() -> usize {
    20
}

fn default_top_n_max_limit() -> usize {
    500
}

/// 載入設定並把 CIDR 字串預先解析好。
///
/// 網段在啟動時就解析完畢，而不是每個請求再 parse：一來省掉熱路徑上的
/// 字串處理，二來設定寫錯時會在啟動階段就失敗，而不是等到某個請求被
/// 誤判為「來源不被允許」才發現。
pub struct LoadedConfig {
    pub config: Config,
    pub trusted_proxies: Vec<IpNetwork>,
    pub threshold_allowed_ips: Vec<IpNetwork>,
    pub top_n_allowed_ips: Vec<IpNetwork>,
}

impl LoadedConfig {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
        Self::from_toml_str(&text)
    }

    /// 與 [`load`](Self::load) 相同，但來源是字串。
    ///
    /// 抽出來是為了讓測試不必落地一個暫存檔——驗證邏輯（空 keys、
    /// 壞掉的 CIDR）全在這裡，落在檔案讀取之後。
    pub fn from_toml_str(text: &str) -> Result<Self, ConfigError> {
        let config: Config = toml::from_str(text)?;

        if config.auth.keys.is_empty() {
            return Err(ConfigError::NoApiKeys);
        }
        if let Some(bad) = config.auth.keys.iter().find(|k| k.key.trim().is_empty()) {
            return Err(ConfigError::EmptyApiKey(bad.name.clone()));
        }

        let trusted_proxies = parse_networks(&config.auth.trusted_proxies, "auth.trusted_proxies")?;
        let threshold_allowed_ips =
            parse_networks(&config.threshold_check.allowed_ips, "threshold_check.allowed_ips")?;
        let top_n_allowed_ips = parse_networks(&config.top_n.allowed_ips, "top_n.allowed_ips")?;

        Ok(Self {
            config,
            trusted_proxies,
            threshold_allowed_ips,
            top_n_allowed_ips,
        })
    }

    pub fn is_trusted_proxy(&self, ip: IpAddr) -> bool {
        self.trusted_proxies.iter().any(|net| net.contains(ip))
    }

    pub fn threshold_check_allows(&self, ip: IpAddr) -> bool {
        self.threshold_allowed_ips.iter().any(|net| net.contains(ip))
    }

    /// 空白名單代表門檻端點未啟用。
    pub fn threshold_check_enabled(&self) -> bool {
        !self.threshold_allowed_ips.is_empty()
    }

    pub fn top_n_allows(&self, ip: IpAddr) -> bool {
        self.top_n_allowed_ips.iter().any(|net| net.contains(ip))
    }

    /// 空白名單代表 Top-N 端點未啟用。
    pub fn top_n_enabled(&self) -> bool {
        !self.top_n_allowed_ips.is_empty()
    }
}

/// 允許省略前綴長度：`10.0.0.5` 等同 `10.0.0.5/32`。
///
/// 設定檔裡寫單一主機是最常見的情況，強迫每個人補 `/32` 只會製造筆誤。
fn parse_networks(raw: &[String], field: &str) -> Result<Vec<IpNetwork>, ConfigError> {
    raw.iter()
        .map(|entry| {
            let entry = entry.trim();
            let parsed = if entry.contains('/') {
                entry.parse::<IpNetwork>().ok()
            } else {
                entry.parse::<IpAddr>().map(IpNetwork::from).ok()
            };
            parsed.ok_or_else(|| ConfigError::BadNetwork {
                field: field.to_string(),
                value: entry.to_string(),
            })
        })
        .collect()
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config file {path}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid config file syntax: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("auth.keys must not be empty; at least one API key is required")]
    NoApiKeys,
    #[error("API key \"{0}\" has an empty key field")]
    EmptyApiKey(String),
    #[error("{field}: \"{value}\" is not a valid IP address or CIDR")]
    BadNetwork { field: String, value: String },
}
