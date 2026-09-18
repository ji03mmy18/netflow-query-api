# netflow-query-api

針對 `netflow-collector` 寫入的統計表提供唯讀查詢的 REST API。
Axum + Tokio + SQLx，連線 PostgreSQL / TimescaleDB。

資料表定義見 [`sql/schema.sql`](sql/schema.sql)（由 collector 端負責建立與維護，本服務不做任何寫入）。

## 用量欄位

所有端點的用量都以同一組五個欄位回傳。回應的 JSON key 統一為 camelCase：

| 欄位 | 資料表欄位 | 方向 |
|---|---|---|
| `internetDownloadBytes` | `ext_rx_bytes` | 外網 → 本機 |
| `internetUploadBytes` | `ext_tx_bytes` | 本機 → 外網 |
| `internetTotalBytes` | — | `internetDownloadBytes + internetUploadBytes`，**不含校內流量** |
| `schoolDownloadBytes` | `intra_rx_bytes` | 內網 → 本機 |
| `schoolUploadBytes` | `intra_tx_bytes` | 本機 → 內網 |

**方向是「受監控主機」的視角，不是交換器介面的視角。** 這兩種視角在網路工具裡都存在且方向相反，是經典的誤解來源——`internetDownloadBytes` 是該主機下載了多少，`internetUploadBytes` 是該主機上傳了多少。

**`internetTotalBytes` 只計對外流量**（`internetDownloadBytes + internetUploadBytes`）。過量門檻檢查比對的就是這個數字。刻意不提供 `schoolTotalBytes`：校內流量跨主機不可加總，提供一個合計欄位只會鼓勵誤用。

### school_* 的兩個限制

`school*` 有 schema 註解點明的兩個語意限制，使用前務必理解：

1. **母體結構性不完整。** 只涵蓋「經過核心交換器的內部流量」。在邊緣交換器就被 L3 轉發掉的部分完全不在其中。
2. **跨主機不可加總。** 若一筆內部流量的兩端都在監控清單中，同一份流量會同時計入 A 的 `schoolUploadBytes` 與 B 的 `schoolDownloadBytes`。因此把多台主機的 `school*` 加起來，不等於實際內網流量。

對外用量請一律使用 `internetDownloadBytes` / `internetUploadBytes` / `internetTotalBytes`。

## 快速開始

```sh
cp config.example.toml config.toml            # 修改資料庫連線與 API Key
./netflow-query-api --check-config            # 部署前先確認設定檔沒寫錯
./netflow-query-api
```

預設監聽 `0.0.0.0:8081`。日誌層級可用 `RUST_LOG` 覆寫（例如 `RUST_LOG=debug`）。

## 命令列介面

```
Usage: netflow-query-api [OPTIONS]

Options:
  -c, --config <PATH>  設定檔路徑 [default: config.toml]
      --check-config   只驗證設定檔後結束，不連線資料庫、不監聽埠號
      --check-dbconn   連同資料庫連線一起驗證（須搭配 --check-config）
  -h, --help           Print help (see more with '--help')
  -V, --version        Print version
```

`--check-config` 會印出解析後的設定摘要並以結束碼表示結果（0 = 通過），
適合放進部署腳本。它印的是**實際生效的值**，不是設定檔的原文——例如
白名單裡寫 `10.0.0.5` 會顯示為 `10.0.0.5/32`，`threshold_check.allowed_ips`
留空會直接標示「門檻檢查端點停用」。

```
$ netflow-query-api --check-config
設定檔 config.toml 檢查通過
  監聽位址        0.0.0.0:8081
  資料庫          localhost:5432/netflow（user=netflow_ro, max_connections=5）
  API Key         2 把：dashboard, monitor
  受信任代理      無（一律以 TCP 對端 IP 為準）
  門檻檢查來源    10.0.0.5/32, 192.168.1.0/24
  單次 IP 上限    50
  分佈回溯上限    395 天
```

`--check-dbconn` 必須搭配 `--check-config`，會在設定檢查通過後實際連一次
資料庫並下一道查詢——連線建立成功不代表這條 session 能用（權限、
`search_path` 之類的問題要實際查詢才會浮現）。

## 認證

所有 `/api/v1/*` 端點都需要 header：

```
X-API-Key: <config.toml 中 auth.keys 的某一把>
```

`/healthz` 不需要認證。

`GET /api/v1/usage/exceeded` 額外要求來源 IP 落在 `[threshold_check].allowed_ips`；該陣列為空時端點一律拒絕（fail-closed）。

若服務跑在反向代理後面，必須把代理的位址填進 `auth.trusted_proxies`，否則來源 IP 會被判定為代理自己的 IP。反之，**不要**把不受你控制的來源放進 `trusted_proxies`——那等於允許對方用 `X-Forwarded-For` 偽造自己的 IP。

## 端點

### 1. 當日用量查詢

```
GET /api/v1/usage/today?ip=10.1.2.3&ip=10.1.2.4
GET /api/v1/usage/today?ip=10.1.2.3,10.1.2.4      # 逗號分隔亦可
```

單次上限 `limits.max_ips_per_request`（預設 50）。重複的 IP 會去重。
送出的每個 IP 都會有一筆結果，沒有資料的補 0。

```json
{
  "date": "2026-09-18",
  "results": [
    {
      "ip": "10.1.2.3",
      "internetDownloadBytes": 4705537592,
      "internetUploadBytes": 118372641,
      "internetTotalBytes": 4823910233,
      "schoolDownloadBytes": 82134901,
      "schoolUploadBytes": 9927430
    },
    {
      "ip": "10.1.2.4",
      "internetDownloadBytes": 0,
      "internetUploadBytes": 0,
      "internetTotalBytes": 0,
      "schoolDownloadBytes": 0,
      "schoolUploadBytes": 0
    }
  ]
}
```

### 2. 歷史一週用量查詢

```
GET /api/v1/usage/week?ip=10.1.2.3
```

固定 7 筆，由舊到新（前六日 → 當日），缺漏日期補 0。以下範例僅列出前兩筆。

```json
{
  "ip": "10.1.2.3",
  "days": [
    {
      "day": "2026-09-12",
      "internetDownloadBytes": 3102773301,
      "internetUploadBytes": 99110811,
      "internetTotalBytes": 3201884112,
      "schoolDownloadBytes": 41028833,
      "schoolUploadBytes": 5583920
    },
    {
      "day": "2026-09-13",
      "internetDownloadBytes": 0,
      "internetUploadBytes": 0,
      "internetTotalBytes": 0,
      "schoolDownloadBytes": 0,
      "schoolUploadBytes": 0
    }
  ]
}
```

### 3. 一日用量分佈

```
GET /api/v1/usage/daily?ip=10.1.2.3&date=2026-09-17
GET /api/v1/usage/daily?ip=10.1.2.3                 # date 省略 = 當日
```

5 分鐘刻度，固定 288 筆（缺漏補 0）；查當日時只回到目前所在的刻度為止。以下範例僅列出前兩筆。

`flow_stat_5m` 有 retention policy，超出 `limits.distribution_max_age_days`（預設 395 天）的日期會回 `422`，而不是靜默回傳一整片 0。

```json
{
  "ip": "10.1.2.3",
  "date": "2026-09-17",
  "bucketSeconds": 300,
  "points": [
    {
      "ts": "2026-09-17T00:00:00+08:00",
      "internetDownloadBytes": 11204,
      "internetUploadBytes": 839,
      "internetTotalBytes": 12043,
      "schoolDownloadBytes": 0,
      "schoolUploadBytes": 512
    },
    {
      "ts": "2026-09-17T00:05:00+08:00",
      "internetDownloadBytes": 0,
      "internetUploadBytes": 0,
      "internetTotalBytes": 0,
      "schoolDownloadBytes": 0,
      "schoolUploadBytes": 0
    }
  ]
}
```

### 4. 過量門檻檢查

```
GET /api/v1/usage/exceeded?threshold_mib=1024
GET /api/v1/usage/exceeded?thresholdMib=1024      # 兩種寫法都接受
```

回傳當日 `internetTotalBytes`（對外合計）**超過**門檻的所有 IP，依 `internetTotalBytes` 由大到小排序。
門檻固定比對對外流量，`school*` 不納入計算。需要 API Key + 來源 IP 白名單。

```json
{
  "date": "2026-09-18",
  "thresholdMib": 1024.0,
  "thresholdBytes": 1073741824,
  "count": 2,
  "results": [
    {
      "ip": "10.1.2.3",
      "internetDownloadBytes": 4705537592,
      "internetUploadBytes": 118372641,
      "internetTotalBytes": 4823910233,
      "schoolDownloadBytes": 82134901,
      "schoolUploadBytes": 9927430
    },
    {
      "ip": "10.1.9.8",
      "internetDownloadBytes": 201338880,
      "internetUploadBytes": 973066240,
      "internetTotalBytes": 1174405120,
      "schoolDownloadBytes": 0,
      "schoolUploadBytes": 0
    }
  ]
}
```

## 錯誤格式

```json
{ "error": { "code": "VALIDATION_ERROR", "message": "..." } }
```

| code | HTTP |
|---|---|
| `UNAUTHORIZED` | 401 |
| `FORBIDDEN` | 403 |
| `VALIDATION_ERROR` | 422 |
| `DATABASE_ERROR` | 500 |

## 部署（systemd）

[`netflow-query-api.service.example`](netflow-query-api.service.example) 是可直接使用的
unit 範例，預設路徑為 `/opt/netflow-query-api`、執行身分為 `netflow`。檔案開頭有完整
安裝步驟，摘要如下：

```sh
sudo useradd --system --no-create-home --shell /usr/sbin/nologin netflow
sudo mkdir -p /opt/netflow-query-api
sudo tar -xzf netflow-query-api-linux-amd64.tar.gz -C /opt/netflow-query-api
sudo cp /opt/netflow-query-api/config.example.toml /opt/netflow-query-api/config.toml
sudo $EDITOR /opt/netflow-query-api/config.toml

# config.toml 含資料庫密碼與 API Key
sudo chown -R root:netflow /opt/netflow-query-api
sudo chmod 640 /opt/netflow-query-api/config.toml

sudo cp netflow-query-api.service.example /etc/systemd/system/netflow-query-api.service
sudo systemctl daemon-reload
sudo systemctl enable --now netflow-query-api
```

檔案擁有者刻意設為 `root`、群組為 `netflow`：服務只需要讀取，不該有能力覆寫
自己的執行檔或設定檔。

unit 裡有幾個值得知道的設定：

- **`ExecStartPre` 會先跑 `--check-config`**，設定寫錯時在啟動前就失敗，並把實際
  生效的值印進 journal。沒有這行的話，設定問題只會留下一行啟動失敗訊息，看不到
  其他設定被解讀成什麼。
- **資料庫未就緒不需要在 unit 裡描述開機順序**。服務會在連線逾時（10 秒）後結束，
  靠 `Restart=on-failure` + `RestartSec=5` 自行收斂。
- **`RestrictAddressFamilies` 必須包含 `AF_NETLINK`**。glibc 的 `getaddrinfo()` 會用
  netlink 列舉本機網路介面，少了它，設定檔中資料庫若以主機名稱（而非 IP）指定
  會解析失敗——而症狀看起來像 DNS 壞了，很難聯想到是 unit 的限制造成的。

服務同時處理 SIGINT 與 SIGTERM，`systemctl stop` 會觸發 graceful shutdown
（停止接受新連線、等現有請求結束）。可在 journal 中確認：

```sh
sudo systemctl stop netflow-query-api
journalctl -u netflow-query-api -n 5
# 應看到 "SIGTERM received; shutting down" 與 "shutdown complete"
```

## 建置與發佈

本機建置：

```sh
cargo build --release      # 產物在 target/release/netflow-query-api
```

發佈由 GitHub Actions 手動觸發：Actions → **Build Linux binaries** → Run workflow。

| 輸入 | 說明 |
|---|---|
| `ref` | 要編譯的 branch / tag / commit，留空用預設分支 |
| `tag` | Release 標籤如 `v0.1.0`。**留空則只產生 artifact，不建立 Release** |

流程會在 amd64 與 arm64 上平行建置，各打包成 `netflow-query-api-linux-<arch>.tar.gz`
（含執行檔、`config.example.toml`、`README.md`）。有給 `tag` 時再加上一份
`SHA256SUMS` 建立 Release，部署後可用 `sha256sum -c` 驗證傳輸完整性。

**建置基底刻意選 Debian 12**（容器用官方的 `rust:1-bookworm`）。執行檔會連結
建置環境的 glibc，而 glibc 只保證向前相容：用舊的編、在新系統上可以跑，反過來
不行。Debian 12 是 glibc 2.36、Debian 13 是 2.41，所以這裡編出來的東西兩個版本
都能跑。若改用 runner 預設的 Ubuntu 24.04（glibc 2.39），產出的執行檔在 Debian 12
就跑不起來，而錯誤訊息只有一行 `GLIBC_2.39 not found`，看不出是建置環境選錯。

用容器的另一個好處是 runner 映像的版本完全不影響產出——之後 GitHub 汰換 runner
也不會改變執行檔的相容性。

每次建置會在 Actions 的 Step Summary 印出實際量到的最低 glibc 需求（由 `objdump -T`
推算），可直接和目標機器的 `ldd --version` 對照。

arm64 使用 GitHub 的原生 arm64 runner 而非交叉編譯：TLS 走 rustls，底層的 `ring`
含 C 與組語，交叉編譯需要另外準備 aarch64 toolchain 並設定 linker。原生 runner
讓這類問題不存在（公開 repo 可免費使用）。

## 設計註記

**「當日」在應用層計算，不用 `CURRENT_DATE`。** schema 裡的 `ALTER DATABASE ... SET timezone TO 'Asia/Taipei'` 在連線帳號不是資料庫擁有者時只發 NOTICE 就跳過，不會報錯。一旦沒套用成功，`CURRENT_DATE` 退回 UTC，台灣時間凌晨 0~8 點會查到前一天的資料且完全靜默。日期改由 `chrono-tz` 在 Rust 端算出後綁定為查詢參數，這個失效模式就不存在。

**補零在 Rust 端做，不用 `time_bucket_gapfill`。** `flow_stat_1d` 是普通表、`day` 是 `date` 型別，gapfill 用起來不順；而且多 IP 查詢時整個區間都沒資料的 IP 不會進 gapfill 的輸出，仍得在應用層補。點數本來就有上限（7 / 288），統一在一處補完邏輯最單純。

**索引使用。** 四支查詢都吃得到既有索引：`flow_stat_1d` 的 PK `(day, addr)` 涵蓋當日查詢與門檻掃描，`idx_1d_ip_day` 涵蓋一週查詢，`idx_5m_ip_time` 涵蓋一日分佈。沒有需要新增的索引，schema 不必改動。
