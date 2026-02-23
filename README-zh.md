# mcp-wrapper-rs

輕量級通用 MCP (Model Context Protocol) 代理，使用 Rust 編寫。透過快取協議回應並按需啟動子進程，將記憶體佔用從每個 MCP 伺服器 ~100MB 降至 ~2MB。

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

`mcp-wrapper-rs` 作為透明代理運作：

```
┌─────────────┐      ┌─────────────────┐      ┌─────────────┐
│ Claude Code │ ──── │ mcp-wrapper-rs  │ ──── │ MCP Server  │
│             │      │    (~2MB)       │      │  (按需啟動)  │
└─────────────┘      └─────────────────┘      └─────────────┘
```

- **啟動時**：啟動子進程一次，快取 `tools/list`、`prompts/list`、`resources/list`
- **運行時**：協議查詢從快取即時回應
- **工具呼叫**：維持一個持久化子進程連線，跨呼叫重複使用（失敗時自動重啟）

結果：**4 個 MCP 伺服器僅使用 ~8MB**（原本 ~370MB）

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

# 使用自訂初始化超時（預設 5 秒；啟動較慢的伺服器可調高）
mcp-wrapper-rs --init-timeout 15 npx -y mcp-searxng
```

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
      "args": ["python3", "/path/to/server.py"]
    }
  }
}
```

## 運作原理

1. **初始化階段**
   - 啟動子進程，透過 rmcp SDK 完成 MCP 握手
   - 查詢 `tools/list`、`prompts/list`、`resources/list`、`resources/templates/list`（支援分頁）
   - 每個查詢有可設定的超時（`--init-timeout`，預設 5 秒）；無回應的伺服器會被跳過
   - 快取所有結果，終止初始化子進程

2. **運行階段**
   - `initialize` → 從快取即時回應
   - `tools/list` → 從快取即時回應
   - `prompts/list` → 從快取即時回應
   - `resources/list` → 從快取即時回應
   - `tools/call` → 轉發至持久化後端子進程

3. **資源管理**
   - 持久化後端子進程跨工具呼叫重複使用
   - 後端進程死亡時，下次呼叫自動重啟
   - rmcp SDK 負責進程生命週期和協議細節

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
- 僅在工具執行時活動（按需啟動子進程）

**為何更快**
- **按需啟動子進程**：進程僅在需要時運行，消除閒置開銷
- **快取優先設計**：`initialize`、`tools/list`、`prompts/list`、`resources/list` 從記憶體提供
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

詳見 [ARCHITECTURE.md](ARCHITECTURE.md)（英文）。

## 授權

MIT License - 見 [LICENSE](LICENSE)
