# mcp-wrapper-rs

穩定型 MCP supervisor proxy，使用 Rust 編寫。它把 stdio MCP server 轉成可控制、可觀測的 backend：快取穩定 discovery 資料，透過統一的 `mcp.wrapper` tool 暴露 lifecycle 控制，並只在動態呼叫需要時啟動或重用後端子進程。

[English](README.md)

## 問題背景

MCP 伺服器（特別是基於 npx 的）在持續運行時消耗大量記憶體：

```
npx -y mcp-searxng          ~100MB
npx -y fetcher-mcp          ~120MB
npx -y @oevortex/ddg_search ~100MB
python3 server.py           ~50MB
────────────────────────────────────
總計                         ~370MB（閒置狀態！）
```

## 解決方案

`mcp-wrapper-rs` 作為穩定的 supervisor proxy 運作：

```
┌─────────────┐      ┌──────────────────────┐      ┌─────────────┐
│ Claude Code │ ──── │ mcp-wrapper-rs       │ ──── │ MCP Server  │
│             │      │ cache + mcp.wrapper  │      │  (按需啟動)  │
└─────────────┘      └──────────────────────┘      └─────────────┘
```

- **啟動時**：啟動後端一次，快取穩定 discovery data，並暴露 wrapper-owned tools capability
- **運行時**：`initialize` 和 list 類請求從快取即時回應，`tools/list` 會包含 wrapper-owned `mcp.wrapper`
- **Wrapper 控制**：`mcp.wrapper` action 由 wrapper 本地處理，用於 backend status、ping、refresh、restart、stop
- **後端呼叫**：動態 MCP 工作仍由後端子進程擁有，wrapper 只負責重用或必要時重啟

結果：**4 個 MCP 伺服器僅使用 ~8MB**（原本 ~370MB）

## 核心概念

`mcp-wrapper-rs` 是一個 **supervisor proxy（監督代理）**：客戶端連到一個永不中斷的穩定 MCP server，真正的 MCP server 是它背後被管理的子行程。後端可以啟動、重啟、停止，而那條連線從不斷開。

產品由四個概念定義：

- **Cache-first 閒置經濟**。穩定的探索資料（`initialize`、`tools/list`、`prompts/list`、`resources/list`）只擷取一次並從記憶體服務。後端採惰性啟動——只在真正需要動態呼叫（`tools/call`、`resources/read`…）時才 spawn——所以閒置的 wrapper 幾乎不耗資源。對外服務的 `tools/list` / `initialize` 視圖是 **hold-state**：只在後端資料載入或刷新時重建，不在每次讀取時重算。每次 refresh 先把所有指定探索結果驗證成單一 candidate，再原子替換 committed snapshot；錯誤、過期 revision 或 backend generation 改變都保留上一份真相。

- **唯一統一控制面：`mcp.wrapper`**。wrapper 在 `tools/list` 注入唯一保留工具 `mcp.wrapper`。後端 status、ping、刷新探索、restart、stop 全部是這個工具上的 `action`。Admin CLI 與 MCP 客戶端投影到**同一份** action schema 與結果信封——沒有第二條控制路徑。後端若試圖註冊這個保留名稱，無法覆蓋它。

- **Owner-local 生命週期**。每塊狀態只有一個 owner。`BackendSlot` 擁有後端行程的 alive 判定與 restart/stop 轉移；`RequestLifetime` 以 session-aware request row 擁有請求 identity，並由這些 row 推導 active-call 狀態，因此 cancellation、completion 與 backend generation 不會跨 client 混淆。`Cache` 擁有探索資料與對外視圖的生命週期。`tools` 是對外入口與分派層，擁有 action schema 與信封，但**不**持有任何生命週期狀態——它把 action 路由到 owner method。驗證內化在 invocation 物件本身，而非獨立 validator。

- **Observe → hook → readback 控制面**。後端事實（行程結束、`listChanged`、降級冷卻）是訊號而非真相。控制動作透過 owner 請求一個合法轉移；新狀態在任何客戶端、log、CLI 或工具投影宣告前，先由 readback 快照證明。狀態是證據，不是附在指令上的期望。

在 **daemon 模式**下，單一 broker 擁有共享的 backend/cache 配對並 fan-out 給多個客戶端 session。Client identity 使用 `(session_id, client_request_id)` 複合鍵，而不是 process-global JSON-RPC ID；broker 再將它 join 到 backend request identity 與 generation。Active-call 安全由仍存活的 request row 推導，所以一個 session 不能取消另一個 session 的請求，也不能在尚有工作進行時 stop 或 restart 共享後端。

### 補足官方 stdio MCP 的不足

官方 stdio MCP 留下真實缺口。本 wrapper 在單一穩定 transport 後補上它們：

| 官方 stdio MCP 的不足 | mcp-wrapper-rs 補上 |
|---|---|
| Client 無法控制所連的 server | `mcp.wrapper` 生命週期動作：`restart` / `stop` / `refresh` / `status` / `ping`，且重啟時 transport 不斷線 |
| 沒有標準的 backend 狀態 readback | `observe → hook → readback` 控制面：snapshot、generation、cache epoch、discovery hash 作為已證據化的狀態 |
| 閒置 server 仍佔記憶體 | Cache-first 穩定探索層 + 後端惰性啟動 |
| 每個 client 各自 spawn server | Daemon broker 讓多 session 共享單一 backend/cache |

**下一個開發方向：** 更深的**雙向溝通**橋接，讓被包裝的 server 能透過穩定 wrapper transport 驅動更豐富的雙向（server 對 client）MCP 互動。

完整架構、owner map、flow projection 與 proof gate 見 [handbook](docs/handbook/index.html)。

## 安裝

### 從原始碼編譯

```bash
git clone https://github.com/woolkingx/mcp-wrapper-rs.git
cd mcp-wrapper-rs
cargo install --path .
```

這將以 release 模式編譯並安裝二進位檔案到 `~/.cargo/bin/mcp-wrapper-rs`（約 440KB）。

**開發時**: 使用 `cargo build --release` 編譯但不安裝。二進位檔案位於 `target/release/mcp-wrapper-rs`。

### 預編譯二進位

查看 [Releases](https://github.com/woolkingx/mcp-wrapper-rs/releases) 獲取預編譯版本。

## 使用方法

```bash
mcp-wrapper-rs [--init-timeout <秒>] <命令> [參數...]
```

### 範例

```bash
# 包裝基於 npx 的 MCP 伺服器
mcp-wrapper-rs npx -y mcp-searxng

# 包裝 Python MCP 伺服器
mcp-wrapper-rs python3 /path/to/server.py

# 使用環境變數（從父進程繼承）
SEARXNG_URL=http://localhost:8080 mcp-wrapper-rs npx -y mcp-searxng

# 使用自訂初始化超時（預設 30 秒；啟動較慢的伺服器可調高）
mcp-wrapper-rs --init-timeout 15 npx -y mcp-searxng

# Daemon 模式：多個 client 共用同一個 broker/backend
mcp-wrapper-rs --daemon python3 /path/to/server.py
```

### Admin CLI

Admin CLI 是同一套 `mcp.wrapper` action schema、daemon、broker、backend、MCP cache owner 的狀態投影與控制入口，不是第二套後端管理器。

```bash
# 查看某個 command identity 的 broker/backend 狀態
mcp-wrapper-rs status --json -- python3 /path/to/server.py

# 需要時啟動 broker，重啟 backend，並刷新 MCP cache data
mcp-wrapper-rs backend restart --start --json -- python3 /path/to/server.py

# 查看或停止該 command identity 對應的 broker
mcp-wrapper-rs broker status --json -- python3 /path/to/server.py
mcp-wrapper-rs broker stop --json -- python3 /path/to/server.py
```

唯讀 admin command 不會啟動 broker，除非明確加上 `--start`。

### Claude Code 設定

編輯 `~/.claude.json`：

```json
{
  "mcpServers": {
    "searxng": {
      "type": "stdio",
      "command": "/path/to/mcp-wrapper-rs",
      "args": ["npx", "-y", "mcp-searxng"],
      "env": {
        "SEARXNG_URL": "http://localhost:8080"
      }
    },
    "fetcher": {
      "type": "stdio",
      "command": "/path/to/mcp-wrapper-rs",
      "args": ["npx", "-y", "fetcher-mcp"]
    },
    "my-python-server": {
      "type": "stdio",
      "command": "/path/to/mcp-wrapper-rs",
      "args": ["--daemon", "python3", "/path/to/server.py"]
    }
  }
}
```

## 運作原理

1. **初始化階段**
   - 啟動子進程，透過 raw JSON-RPC 完成 MCP 握手
   - 查詢 `tools/list`、`prompts/list`、`resources/list`、`resources/templates/list`
   - 每個查詢有可設定的超時（`--init-timeout`，預設 30 秒）；無回應的伺服器會被跳過
   - 快取所有結果，終止初始化子進程

2. **運行階段**
   - `initialize` → 從快取即時回應
   - `tools/list` → 從快取即時回應，並包含 `mcp.wrapper`
   - `prompts/list` → 從快取即時回應
   - `resources/list` → 從快取即時回應
   - `tools/call` + `mcp.wrapper` → 由 `src/tools` 本地執行 wrapper control/readback
   - 其他 `tools/call`、`resources/read`、`prompts/get` → 轉發至持久化後端子進程

3. **資源管理**
   - 持久化後端子進程跨工具呼叫重複使用
   - 後端進程死亡時，下次呼叫自動重啟
   - wrapper 負責 process group 清理、backend lifecycle、cache refresh readback

## 除錯日誌

除錯日誌**預設關閉**。使用 `MCP_WRAPPER_DEBUG` 環境變數啟用：

```bash
MCP_WRAPPER_DEBUG=1 mcp-wrapper-rs npx -y mcp-searxng
```

每個 MCP 伺服器根據推斷的名稱使用獨立的日誌檔。日誌位置遵循 XDG Base Directory 規範：
- `$XDG_RUNTIME_DIR/mcp-wrapper/mcp-searxng.log`（Linux 有 XDG runtime dir 時）
- `$TMPDIR/mcp-wrapper/mcp-searxng.log`（macOS 或自訂 TMPDIR）
- `/tmp/mcp-wrapper/mcp-searxng.log`（後備路徑）

可使用 `MCP_SERVER_NAME` 覆蓋名稱：
```bash
MCP_SERVER_NAME=my-custom-name mcp-wrapper-rs python3 server.py
# 日誌寫入: $XDG_RUNTIME_DIR/mcp-wrapper/my-custom-name.log
```

## 效能對比

| 指標 | 之前 | 之後 |
|------|------|------|
| 記憶體（4 個伺服器） | ~370MB | ~8MB |
| 二進位大小 | N/A | ~1.6MB |
| `tools/list` 延遲 | ~2s | <1ms |
| `tools/call` 延遲 | 相同 | 相同 |

### 實際使用效果

在生產環境部署 8 個 MCP 伺服器後的實測數據：

**啟動性能**
- Claude Code 啟動時間：**快約 40%**
- MCP 初始化：從 ~10 秒降至 <1 秒（快取即時回應）

**運行性能**
- CPU 負載：**降低約 40%**（無閒置 MCP 進程）
- 回應延遲：協議查詢從快取即時返回
- 靜態協議請求由 wrapper cache 回答；後端 lifecycle 只在動態 MCP 工作被要求後啟動

**為何更快**
- **Lazy backend lifecycle**：後端進程僅在需要時運行，消除閒置開銷
- **快取優先設計**：`initialize`、`tools/list`、`prompts/list`、`resources/list` 從記憶體提供
- **統一控制面**：CLI 和 MCP client 走同一個 wrapper action boundary 做 lifecycle readback/control
- **持久化後端連線**：工具呼叫重複使用同一子進程，首次呼叫後無重啟開銷

## 相容性

適用於任何符合以下條件的 MCP 伺服器：
- 使用 stdio 傳輸
- 遵循 MCP 協議（JSON-RPC 2.0）
- 支援標準初始化握手

已測試：
- `npx -y mcp-searxng`
- `npx -y fetcher-mcp`
- `npx -y @oevortex/ddg_search`
- Python MCP 伺服器

## 架構設計

目前架構 truth 在 [handbook](docs/handbook/index.html)。`README` 只作為安裝、使用與 repo front page；owner boundary、flow projection、proof gate 以 handbook 為準。舊版 compact note 見 [ARCHITECTURE.md](ARCHITECTURE.md)。

## 授權

MIT License - 見 [LICENSE](LICENSE)
