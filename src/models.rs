//! API 回應結構。
//!
//! 所有回應的 JSON key 統一為 camelCase。Rust 端維持 snake_case 慣例，
//! 轉換交給 `#[serde(rename_all = "camelCase")]`——逐一手寫 `rename`
//! 的話，日後新增欄位很容易漏掉一個，而漏掉不會有任何編譯錯誤。

use serde::Serialize;

/// 一組用量數字，對應資料表的四個 bytes 欄位加上對外合計。
///
/// 方向是「**受監控主機**」的視角，不是交換器介面的視角——這兩種視角在
/// 網路工具裡都存在且方向相反，是經典的誤解來源：
///
/// | JSON key | 資料表欄位 | 方向 |
/// |---|---|---|
/// | `internetDownloadBytes` | `ext_rx_bytes`   | 外網 → 本機 |
/// | `internetUploadBytes`   | `ext_tx_bytes`   | 本機 → 外網 |
/// | `schoolDownloadBytes`   | `intra_rx_bytes` | 內網 → 本機 |
/// | `schoolUploadBytes`     | `intra_tx_bytes` | 本機 → 內網 |
///
/// `internetTotalBytes` 是對外合計（download + upload），不含校內流量。
/// 門檻檢查比對的就是這個數字。
///
/// ⚠ `school*` 有兩個 schema 註解點明的限制，使用前務必理解：
///   1. 母體只有「經過核心交換器的內部流量」。在邊緣交換器就被 L3 轉發
///      掉的部分完全不在其中，所以它是結構性不完整的量測。
///   2. 若一筆內部流量的兩端都在監控清單中，同一份流量會同時計入 A 的
///      `schoolUploadBytes` 與 B 的 `schoolDownloadBytes`。因此跨主機
///      加總 `school*` 不等於實際內網流量。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub internet_download_bytes: i64,
    pub internet_upload_bytes: i64,
    /// 對外合計：download + upload（不含 `school*`）
    pub internet_total_bytes: i64,
    pub school_download_bytes: i64,
    pub school_upload_bytes: i64,
}

impl Usage {
    pub fn new(ext_rx: i64, ext_tx: i64, intra_rx: i64, intra_tx: i64) -> Self {
        Self {
            internet_download_bytes: ext_rx,
            internet_upload_bytes: ext_tx,
            // saturating：bigint 相加理論上可能溢位 i64。實務上不會發生
            // （i64 上限約 9.2 EB），但溢位 panic 會讓整支查詢失敗，
            // 飽和至上限至少讓其他 IP 的數字照樣回得出去。
            internet_total_bytes: ext_rx.saturating_add(ext_tx),
            school_download_bytes: intra_rx,
            school_upload_bytes: intra_tx,
        }
    }

    /// 補零用：沒有資料的時段/IP。
    pub const fn zero() -> Self {
        Self {
            internet_download_bytes: 0,
            internet_upload_bytes: 0,
            internet_total_bytes: 0,
            school_download_bytes: 0,
            school_upload_bytes: 0,
        }
    }
}

/// 單一 IP 的用量。
///
/// `flatten` 讓 JSON 保持扁平（`{"ip":..., "internetDownloadBytes":..., ...}`），
/// 不變成嵌套物件：扁平欄位對 Grafana / LibreNMS 這類消費端友善，
/// CSV 匯出也自然。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IpUsage {
    pub ip: String,
    #[serde(flatten)]
    pub usage: Usage,
}

/// 一、當日用量查詢
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TodayUsage {
    /// 台北時區的今天
    pub date: String,
    pub results: Vec<IpUsage>,
}

/// 二、歷史一週用量查詢
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WeekUsage {
    pub ip: String,
    /// 固定 7 筆，由舊到新（前六日 → 當日）
    pub days: Vec<DayUsage>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DayUsage {
    pub day: String,
    #[serde(flatten)]
    pub usage: Usage,
}

/// 三、一日用量分佈
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DailyDistribution {
    pub ip: String,
    pub date: String,
    pub bucket_seconds: i64,
    /// 一般日期固定 288 筆；查當日則只到目前所在的刻度為止
    pub points: Vec<BucketUsage>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BucketUsage {
    /// 分桶起始時刻，RFC3339 帶 +08:00
    pub ts: String,
    #[serde(flatten)]
    pub usage: Usage,
}

/// 四、過量門檻檢查
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExceededReport {
    pub date: String,
    pub threshold_mib: f64,
    pub threshold_bytes: i64,
    pub count: usize,
    /// 依 `internetTotalBytes` 由大到小排序
    pub results: Vec<IpUsage>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `#[serde(flatten)]` 若失效，輸出會靜默變成 `{"ip":..., "usage":{...}}`
    /// 這種嵌套形狀——編譯不會有任何抱怨，呼叫端才會發現取不到欄位。
    /// 這個斷言同時鎖住 camelCase 的轉換結果。
    #[test]
    fn ip_usage_serialises_flat_camel_case() {
        let value = serde_json::to_value(IpUsage {
            ip: "10.1.2.3".to_string(),
            usage: Usage::new(100, 20, 7, 3),
        })
        .unwrap();

        assert_eq!(
            value,
            serde_json::json!({
                "ip": "10.1.2.3",
                "internetDownloadBytes": 100,
                "internetUploadBytes": 20,
                "internetTotalBytes": 120,
                "schoolDownloadBytes": 7,
                "schoolUploadBytes": 3,
            })
        );
    }

    /// internetTotalBytes 的語意固定為「對外合計」。校內流量再大都不該
    /// 影響它——門檻檢查比對的就是這個數字。
    #[test]
    fn internet_total_excludes_school_traffic() {
        let usage = Usage::new(100, 20, 999_999, 888_888);
        assert_eq!(usage.internet_total_bytes, 120);
    }

    #[test]
    fn zero_is_all_zero() {
        let value = serde_json::to_value(Usage::zero()).unwrap();
        for (_, v) in value.as_object().unwrap() {
            assert_eq!(v.as_i64(), Some(0));
        }
    }

    /// 補零後的時段仍要是扁平形狀，且帶著自己的時間戳。
    #[test]
    fn bucket_usage_serialises_flat() {
        let value = serde_json::to_value(BucketUsage {
            ts: "2026-09-18T00:05:00+08:00".to_string(),
            usage: Usage::zero(),
        })
        .unwrap();

        assert_eq!(value["ts"], "2026-09-18T00:05:00+08:00");
        assert_eq!(value["internetTotalBytes"], 0);
        assert!(value.get("usage").is_none(), "不該出現嵌套的 usage 物件");
    }

    /// 掃過所有回應結構的 key，確保沒有漏掉 camelCase 轉換。
    /// 新增欄位時若忘了 rename_all，這個測試會抓到。
    #[test]
    fn no_response_key_uses_snake_case() {
        let distribution = serde_json::to_value(DailyDistribution {
            ip: "10.1.2.3".to_string(),
            date: "2026-09-18".to_string(),
            bucket_seconds: 300,
            points: vec![BucketUsage {
                ts: "2026-09-18T00:00:00+08:00".to_string(),
                usage: Usage::new(1, 2, 3, 4),
            }],
        })
        .unwrap();

        let report = serde_json::to_value(ExceededReport {
            date: "2026-09-18".to_string(),
            threshold_mib: 1024.0,
            threshold_bytes: 1_073_741_824,
            count: 0,
            results: vec![],
        })
        .unwrap();

        let week = serde_json::to_value(WeekUsage {
            ip: "10.1.2.3".to_string(),
            days: vec![DayUsage {
                day: "2026-09-18".to_string(),
                usage: Usage::zero(),
            }],
        })
        .unwrap();

        for value in [&distribution, &report, &week] {
            assert_no_underscores(value);
        }

        // 順手釘住兩個原本是 snake_case 的 key
        assert_eq!(distribution["bucketSeconds"], 300);
        assert_eq!(report["thresholdBytes"], 1_073_741_824i64);
    }

    fn assert_no_underscores(value: &serde_json::Value) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, child) in map {
                    assert!(!key.contains('_'), "key \"{key}\" 仍是 snake_case");
                    assert_no_underscores(child);
                }
            }
            serde_json::Value::Array(items) => items.iter().for_each(assert_no_underscores),
            _ => {}
        }
    }
}
