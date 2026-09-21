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

/// 五、Top-N 用量排行
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TopUsage {
    pub date: String,
    /// 實際生效的筆數上限（呼叫端未指定時為設定檔的預設值）
    pub limit: usize,
    /// 實際回傳的筆數。當日有對外流量的主機不足 `limit` 時會小於 `limit`。
    pub count: usize,
    /// 依 `internetTotalBytes` 由大到小排序
    pub results: Vec<IpUsage>,
}

pub const MIB: i64 = 1024 * 1024;
pub const GIB: i64 = 1024 * MIB;

/// 四、過量門檻檢查的一列。
///
/// 比 [`IpUsage`] 多了「超標多少」。三個 `overThreshold*` 是同一個數字的
/// 三種表示，`overThresholdBytes` 是權威值，另外兩個是方便閱讀的換算。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExceededEntry {
    pub ip: String,
    #[serde(flatten)]
    pub usage: Usage,
    /// `internetTotalBytes - thresholdBytes`
    pub over_threshold_bytes: i64,
    /// 同上，換算為 MiB 後**無條件捨去**。
    ///
    /// ⚠ 剛好超標一點點時這裡會是 0（例如超標 500 KB）。這不是錯誤——
    /// 0 的讀法是「不到一個完整單位」，精確值一律看 `overThresholdBytes`。
    pub over_threshold_mib: i64,
    /// 同上，換算為 GiB 後無條件捨去。門檻若設在 GiB 等級，這個欄位
    /// 多數時候會是 0，只有超標超過 1 GiB 才看得到非零值。
    pub over_threshold_gib: i64,
}

impl ExceededEntry {
    pub fn new(ip: String, usage: Usage, threshold_bytes: i64) -> Self {
        // SQL 已經濾掉未超標的列，理論上恆為正；仍用 saturating + max(0)
        // 收尾，避免日後有人改了篩選條件卻忘了這裡會變成負數。
        let over = usage
            .internet_total_bytes
            .saturating_sub(threshold_bytes)
            .max(0);

        Self {
            ip,
            usage,
            over_threshold_bytes: over,
            // over >= 0，整數除法即為 floor。
            over_threshold_mib: over / MIB,
            over_threshold_gib: over / GIB,
        }
    }
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
    pub results: Vec<ExceededEntry>,
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
            count: 1,
            results: vec![ExceededEntry::new(
                "10.1.2.3".to_string(),
                Usage::new(4_705_537_592, 118_372_641, 0, 0),
                1_073_741_824,
            )],
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

    #[test]
    fn exceeded_entry_computes_overage() {
        // 門檻 1024 MiB，用量 4823910233 bytes
        let entry = ExceededEntry::new(
            "10.1.2.3".to_string(),
            Usage::new(4_705_537_592, 118_372_641, 0, 0),
            1_073_741_824,
        );

        assert_eq!(entry.usage.internet_total_bytes, 4_823_910_233);
        assert_eq!(entry.over_threshold_bytes, 3_750_168_409);
        assert_eq!(entry.over_threshold_mib, 3_576);
        assert_eq!(entry.over_threshold_gib, 3);
    }

    /// 剛好超標一點點時，MiB 與 GiB 無條件捨去後都是 0。
    /// 這是 floor 的必然結果——0 的讀法是「不到一個完整單位」，
    /// 精確值看 overThresholdBytes。
    #[test]
    fn exceeded_entry_floors_partial_units_to_zero() {
        let threshold = 1_073_741_824; // 1 GiB
        let entry = ExceededEntry::new(
            "10.1.2.4".to_string(),
            Usage::new(threshold + 524_288, 0, 0, 0), // 超標 512 KiB
            threshold,
        );

        assert_eq!(entry.over_threshold_bytes, 524_288);
        assert_eq!(entry.over_threshold_mib, 0);
        assert_eq!(entry.over_threshold_gib, 0);
    }

    /// SQL 已濾掉未超標的列，但這裡不該因為上游改動就吐出負數。
    #[test]
    fn exceeded_entry_never_reports_negative_overage() {
        let entry = ExceededEntry::new("10.1.2.5".to_string(), Usage::new(10, 0, 0, 0), 1_000);

        assert_eq!(entry.over_threshold_bytes, 0);
        assert_eq!(entry.over_threshold_mib, 0);
        assert_eq!(entry.over_threshold_gib, 0);
    }

    #[test]
    fn exceeded_entry_serialises_flat() {
        let value = serde_json::to_value(ExceededEntry::new(
            "10.1.2.3".to_string(),
            Usage::new(100, 20, 7, 3),
            50,
        ))
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
                "overThresholdBytes": 70,
                "overThresholdMib": 0,
                "overThresholdGib": 0,
            })
        );
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
