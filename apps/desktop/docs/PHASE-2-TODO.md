# Phase 2 前必修 followups（Phase 1 review 逮到）

> **状态：三条均已修（2026-07-17）。** Phase 2 可以开工。

Phase 1 是本机·只读 MVP，以下三条在 Phase 1 不可利用/低风险，但 **Phase 2 引入自由输入/写入前必须修**：

1. ~~**`read_page` 的 path 未 URL 编码**（`src-tauri/src/api_client.rs`，read_page）。~~ **已修**：
   拒绝前导 `/` 与 `..` 段，其余按段 percent-encode（保留 `/`，兼容 daemon 的 `{*path}` 通配路由）；
   附 `encode_path` / 路径拒绝单元测试。

2. ~~**搜索结果 `{@html h.snippet}` 的注入面**（`src/lib/SearchResults.svelte`）。~~ **已修**：
   `renderSnippet` 先转义 `& < >`，再只白名单放行 `<mark>`/`</mark>`（带属性的 `<mark …>` 不放行）。

3. ~~**MCP `notifications/initialized` 样板重复 + 吞错**（`src-tauri/src/api_client.rs`，semantic_search）。~~ **已修**：
   抽出 `mcp_post`（统一构造 + 查 HTTP 状态），notification 与 `mcp_call` 共用；
   `semantic_search` 在解析 result 前先 surface `resp["error"]["message"]`。

（来源：Phase 1 code review + 前端 subagent 自查。）
