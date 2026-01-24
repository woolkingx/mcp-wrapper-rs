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
- **工具呼叫**：啟動子進程 → 執行 → 返回結果 → 終止子進程

結果：**4 個 MCP 伺服器僅使用 ~8MB**（原本 ~370MB）

## 安裝

### 從原始碼編譯

```bash
git clone https://github.com/woolkingx/mcp-wrapper-rs.git
cd mcp-wrapper-rs
cargo build --release
```

二進位檔案位於 `target/release/mcp-wrapper-rs`（約 432KB）

### 預編譯二進位

查看 [Releases](https://github.com/woolkingx/mcp-wrapper-rs/releases) 獲取預編譯版本。

## 使用方法

```bash
mcp-wrapper-rs <命令> [參數...]
```

### 範例

```bash
# 包裝基於 npx 的 MCP 伺服器
mcp-wrapper-rs npx -y mcp-searxng

# 包裝 Python MCP 伺服器
mcp-wrapper-rs python3 /path/to/server.py

# 使用環境變數（從父進程繼承）
SEARXNG_URL=http://localhost:8080 mcp-wrapper-rs npx -y mcp-searxng
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
   - 啟動子進程，發送 `initialize` + `tools/list` + `prompts/list` + `resources/list`
   - 等待 4 個回應（60 秒超時）
   - 快取所有回應，終止子進程

2. **運行階段**
   - `initialize` → 從快取即時回應
   - `tools/list` → 從快取即時回應
   - `prompts/list` → 從快取即時回應
   - `resources/list` → 從快取即時回應
   - `tools/call` → 啟動子進程 → 執行 → 返回 → 終止

3. **資源管理**
   - 子進程僅在 `tools/call` 執行期間存活
   - 60 秒超時防止卡住
   - kill + wait 確保乾淨終止

## 除錯日誌

日誌寫入 `/tmp/mcp-wrapper-rs.log`：

```bash
tail -f /tmp/mcp-wrapper-rs.log
```

## 效能對比

| 指標 | 之前 | 之後 |
|------|------|------|
| 記憶體（4 個伺服器） | ~370MB | ~8MB |
| 二進位大小 | N/A | 432KB |
| `tools/list` 延遲 | ~2s | <1ms |
| `tools/call` 延遲 | 相同 | 相同 |

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
