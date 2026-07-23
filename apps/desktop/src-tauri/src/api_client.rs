use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};

use crate::types::{
    DaemonStatus, EmbedReport, Hit, MemoryHealth, PageDetail, PageSummary, WritePageArgs,
    WritePageResult,
};

const BASE: &str = "http://127.0.0.1:49374";
const WS: &str = "default";
const PROJ: &str = "_global";

// Per-segment encoding: `/` separators stay literal so the daemon's
// `{*path}` wildcard route still matches.
const PATH_SEGMENT: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}');

fn encode_path(path: &str) -> String {
    path.split('/')
        .map(|seg| utf8_percent_encode(seg, PATH_SEGMENT).to_string())
        .collect::<Vec<_>>()
        .join("/")
}

/// Strip a leading YAML frontmatter block. The daemon's `body_markdown`
/// is the raw file (frontmatter included), but `/admin/write-page`
/// re-frames frontmatter server-side — the editor must round-trip plain
/// body content only.
fn strip_frontmatter(raw: &str) -> String {
    let text = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let Some(rest) = text.strip_prefix("---\n") else {
        return raw.to_string();
    };
    match rest.find("\n---") {
        Some(idx) => {
            let after = &rest[idx + 4..];
            match after.strip_prefix('\n') {
                Some(body) => body.trim_start_matches('\n').to_string(),
                None if after.is_empty() => String::new(),
                // "---" was a prefix of e.g. "----": not a closing fence.
                None => raw.to_string(),
            }
        }
        None => raw.to_string(),
    }
}

fn validate_path(path: &str) -> Result<(), String> {
    if path.trim().is_empty() || path.starts_with('/') || path.split('/').any(|seg| seg == "..") {
        return Err(format!("invalid page path: {path}"));
    }
    Ok(())
}

/// Build an error string from a failed response, surfacing the server's
/// JSON `error` field when present instead of just the status code.
async fn err_body(resp: reqwest::Response) -> String {
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let msg = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| v["error"].as_str().map(str::to_string))
        .unwrap_or(text);
    if msg.trim().is_empty() {
        format!("HTTP {status}")
    } else {
        format!("HTTP {status}: {}", msg.trim())
    }
}

pub struct ApiClient {
    http: reqwest::Client,
    base: String,
    ws: String,
    proj: String,
    token: Option<String>,
}

/// Server URL + bearer token from `~/.config/engram/engram.env` (the
/// single-server deployment config); falls back to loopback / no auth.
fn env_config() -> (String, Option<String>) {
    let (mut base, mut token) = (BASE.to_string(), None);
    if let Some(home) = std::env::var_os("HOME") {
        let path = std::path::Path::new(&home).join(".config/engram/engram.env");
        if let Ok(text) = std::fs::read_to_string(path) {
            for line in text.lines() {
                if let Some(v) = line.strip_prefix("ENGRAM_SERVER_URL=") {
                    if !v.trim().is_empty() {
                        base = v.trim().to_string();
                    }
                }
                if let Some(v) = line.strip_prefix("ENGRAM_AUTH_TOKEN=") {
                    if !v.trim().is_empty() {
                        token = Some(v.trim().to_string());
                    }
                }
            }
        }
    }
    (base, token)
}

impl ApiClient {
    pub fn new() -> Self {
        let (base, token) = env_config();
        let mut c = Self::with_target(&base, WS, PROJ);
        c.token = token;
        c
    }

    fn get(&self, url: &str) -> reqwest::RequestBuilder {
        let b = self.http.get(url);
        match &self.token {
            Some(t) => b.bearer_auth(t),
            None => b,
        }
    }

    fn post(&self, url: &str) -> reqwest::RequestBuilder {
        let b = self.http.post(url);
        match &self.token {
            Some(t) => b.bearer_auth(t),
            None => b,
        }
    }

    /// Point the client at a different daemon/scope (scratch-daemon tests
    /// now, peer machines in Phase 3).
    pub fn with_target(base: &str, ws: &str, proj: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            base: base.to_string(),
            ws: ws.to_string(),
            proj: proj.to_string(),
            token: None,
        }
    }

    pub async fn list_pages(&self) -> Result<Vec<PageSummary>, String> {
        let url = format!(
            "{}/api/v1/workspaces/{}/projects/{}/pages",
            self.base, self.ws, self.proj
        );
        let resp = self.get(&url).send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(err_body(resp).await);
        }
        resp.json::<Vec<PageSummary>>()
            .await
            .map_err(|e| e.to_string())
    }

    pub async fn read_page(&self, path: &str) -> Result<PageDetail, String> {
        validate_path(path)?;
        let url = format!(
            "{}/api/v1/workspaces/{}/projects/{}/pages/{}",
            self.base,
            self.ws,
            self.proj,
            encode_path(path)
        );
        let resp = self.get(&url).send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(err_body(resp).await);
        }
        let mut detail: PageDetail = resp.json().await.map_err(|e| e.to_string())?;
        detail.body = strip_frontmatter(&detail.body);
        Ok(detail)
    }

    pub async fn write_page(&self, args: &WritePageArgs) -> Result<WritePageResult, String> {
        validate_path(&args.path)?;
        let mut body = serde_json::json!({
            "workspace": self.ws,
            "project": self.proj,
            "path": args.path,
            "body": args.body,
            "tags": args.tags,
            "pinned": args.pinned,
        });
        if let Some(v) = &args.title {
            body["title"] = serde_json::json!(v);
        }
        if let Some(v) = &args.kind {
            body["kind"] = serde_json::json!(v);
        }
        if let Some(v) = &args.tier {
            body["tier"] = serde_json::json!(v);
        }
        if let Some(v) = &args.frontmatter {
            body["frontmatter"] = serde_json::json!(v);
        }
        let url = format!("{}/admin/write-page", self.base);
        let resp = self
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(err_body(resp).await);
        }
        resp.json().await.map_err(|e| e.to_string())
    }

    pub async fn delete_page(&self, path: &str) -> Result<(), String> {
        validate_path(path)?;
        let body = serde_json::json!({
            "workspace": self.ws,
            "project": self.proj,
            "path": path,
        });
        let url = format!("{}/admin/delete-page", self.base);
        let resp = self
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(err_body(resp).await);
        }
        Ok(())
    }

    pub async fn admin_status(&self) -> Result<serde_json::Value, String> {
        let url = format!("{}/admin/status", self.base);
        let resp = self.get(&url).send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(err_body(resp).await);
        }
        resp.json().await.map_err(|e| e.to_string())
    }

    pub async fn memory_health(&self) -> Result<MemoryHealth, String> {
        let url = format!(
            "{}/api/v1/workspaces/{}/projects/{}/overview?limit=50",
            self.base, self.ws, self.proj
        );
        let resp = self.get(&url).send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(err_body(resp).await);
        }
        let v: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        serde_json::from_value(v["health"].clone()).map_err(|e| e.to_string())
    }

    pub async fn run_embed(&self, reembed: bool, dry_run: bool) -> Result<EmbedReport, String> {
        let body = serde_json::json!({
            "workspace": self.ws,
            "project": self.proj,
            "reembed": reembed,
            "dry_run": dry_run,
        });
        let url = format!("{}/admin/embed", self.base);
        let resp = self
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(err_body(resp).await);
        }
        resp.json().await.map_err(|e| e.to_string())
    }

    pub async fn run_sweep(&self, dry_run: bool) -> Result<serde_json::Value, String> {
        let body = serde_json::json!({
            "workspace": self.ws,
            "project": self.proj,
            "dry_run": dry_run,
        });
        let url = format!("{}/admin/forget-sweep", self.base);
        let resp = self
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(err_body(resp).await);
        }
        resp.json().await.map_err(|e| e.to_string())
    }

    pub async fn backup(&self) -> Result<Vec<u8>, String> {
        let url = format!("{}/admin/backup", self.base);
        let resp = self.post(&url).send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(err_body(resp).await);
        }
        resp.bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| e.to_string())
    }

    async fn mcp_post(&self, body: &serde_json::Value) -> Result<reqwest::Response, String> {
        let url = format!("{}/mcp", self.base);
        let resp = self
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .json(body)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("MCP HTTP {}", resp.status()));
        }
        Ok(resp)
    }

    async fn mcp_call(&self, body: serde_json::Value) -> Result<serde_json::Value, String> {
        let resp = self.mcp_post(&body).await?;
        let text = resp.text().await.map_err(|e| e.to_string())?;
        let payload = text
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .last()
            .map(|s| s.to_string())
            .unwrap_or(text);
        serde_json::from_str(&payload).map_err(|e| e.to_string())
    }

    pub async fn semantic_search(&self, query: &str) -> Result<Vec<Hit>, String> {
        let init = serde_json::json!({
            "jsonrpc":"2.0","id":0,"method":"initialize",
            "params":{"protocolVersion":"2024-11-05","capabilities":{},
                      "clientInfo":{"name":"engram-desktop","version":"0.1"}}});
        let _ = self.mcp_call(init).await?;
        // Notification: no JSON-RPC response body, so post-and-check-status only.
        let notified = serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"});
        let _ = self.mcp_post(&notified).await?;
        let call = serde_json::json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"memory_query","arguments":{
                "query": query, "project": self.proj, "workspace": self.ws, "limit": 10}}});
        let resp = self.mcp_call(call).await?;
        if let Some(msg) = resp["error"]["message"].as_str() {
            return Err(format!("memory_query error: {msg}"));
        }
        let text = resp["result"]["content"][0]["text"]
            .as_str()
            .ok_or("no content in memory_query response")?;
        let parsed: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        serde_json::from_value(parsed["hits"].clone()).map_err(|e| e.to_string())
    }

    /// Workspace project inventory (name + page_count rows) for the
    /// pending-queue fan-out. `/admin/pending-writes` is per-project by
    /// design, so the queue aggregates client-side.
    pub async fn list_projects(&self) -> Result<Vec<serde_json::Value>, String> {
        let url = format!("{}/api/v1/projects?workspace={}", self.base, self.ws);
        let resp = self.get(&url).send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(err_body(resp).await);
        }
        resp.json().await.map_err(|e| e.to_string())
    }

    pub async fn pending_list(&self, project: &str) -> Result<serde_json::Value, String> {
        let url = format!(
            "{}/admin/pending-writes?workspace={}&project={}&status=pending",
            self.base, self.ws, project
        );
        let resp = self.get(&url).send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(err_body(resp).await);
        }
        resp.json().await.map_err(|e| e.to_string())
    }

    pub async fn pending_detail(
        &self,
        project: &str,
        id: &str,
    ) -> Result<serde_json::Value, String> {
        let url = format!(
            "{}/admin/pending-writes/{}?workspace={}&project={}",
            self.base, id, self.ws, project
        );
        let resp = self.get(&url).send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(err_body(resp).await);
        }
        resp.json().await.map_err(|e| e.to_string())
    }

    pub async fn pending_diff(&self, project: &str, id: &str) -> Result<serde_json::Value, String> {
        let url = format!(
            "{}/admin/pending-writes/{}/diff?workspace={}&project={}",
            self.base, id, self.ws, project
        );
        let resp = self.get(&url).send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(err_body(resp).await);
        }
        resp.json().await.map_err(|e| e.to_string())
    }

    pub async fn pending_approve(
        &self,
        project: &str,
        id: &str,
    ) -> Result<serde_json::Value, String> {
        let url = format!(
            "{}/admin/pending-writes/{}/approve?workspace={}&project={}",
            self.base, id, self.ws, project
        );
        let resp = self
            .post(&url)
            .json(&serde_json::json!({}))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(err_body(resp).await);
        }
        resp.json().await.map_err(|e| e.to_string())
    }

    /// Reject with a free-text reason — recorded into the rejection
    /// context that steers future auto-improve proposals.
    pub async fn pending_reject(
        &self,
        project: &str,
        id: &str,
        reason: &str,
    ) -> Result<serde_json::Value, String> {
        let url = format!(
            "{}/admin/pending-writes/{}/reject?workspace={}&project={}",
            self.base, id, self.ws, project
        );
        let resp = self
            .post(&url)
            .json(&serde_json::json!({ "reason": reason }))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(err_body(resp).await);
        }
        resp.json().await.map_err(|e| e.to_string())
    }

    pub async fn daemon_status(&self) -> DaemonStatus {
        let url = format!(
            "{}/api/v1/workspaces/{}/projects/{}/overview?limit=1",
            self.base, self.ws, self.proj
        );
        match self.get(&url).send().await {
            Ok(r) if r.status().is_success() => {
                let v: serde_json::Value = r.json().await.unwrap_or_default();
                let pc = v["briefing"]["counts"]["pages_latest"].as_u64();
                DaemonStatus {
                    reachable: true,
                    page_count: pc,
                }
            }
            _ => DaemonStatus {
                reachable: false,
                page_count: None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_frontmatter_removes_leading_block_only() {
        assert_eq!(
            strip_frontmatter("---\ntype: decision\n---\n\n# T\nbody"),
            "# T\nbody"
        );
        assert_eq!(strip_frontmatter("# no fm\nbody"), "# no fm\nbody");
        assert_eq!(strip_frontmatter("---\nunterminated"), "---\nunterminated");
        assert_eq!(strip_frontmatter("\u{feff}---\na: 1\n---\nx"), "x");
        assert_eq!(strip_frontmatter("---\na: 1\n---"), "");
        assert_eq!(strip_frontmatter("---\na\n----\nx"), "---\na\n----\nx");
    }

    #[test]
    fn encode_path_encodes_segments_keeps_slashes() {
        assert_eq!(
            encode_path("decisions/foo bar.md"),
            "decisions/foo%20bar.md"
        );
        assert_eq!(encode_path("a#b/c?.md"), "a%23b/c%3F.md");
        assert_eq!(encode_path("decisions/foo.md"), "decisions/foo.md");
    }

    #[tokio::test]
    async fn read_page_rejects_traversal_paths() {
        let c = ApiClient::new();
        assert!(c.read_page("../secrets.md").await.is_err());
        assert!(c.read_page("/etc/passwd").await.is_err());
        assert!(c.read_page("a/../b.md").await.is_err());
    }

    #[tokio::test]
    async fn list_pages_returns_global_scope_pages() {
        let c = ApiClient::new();
        let pages = c
            .list_pages()
            .await
            .expect("daemon must be running on :49374");
        assert!(pages.len() >= 40, "expected ~46 pages, got {}", pages.len());
        assert!(pages.iter().any(|p| p.path.contains("anamra")));
    }

    #[tokio::test]
    async fn read_page_returns_body_and_backlinks() {
        let c = ApiClient::new();
        let pages = c.list_pages().await.unwrap();
        let target = pages
            .iter()
            .find(|p| p.path.contains("qmd-self-healing"))
            .expect("page exists");
        let detail = c.read_page(&target.path).await.expect("read ok");
        assert!(!detail.body.is_empty());
        assert!(detail.title.len() > 0);
    }

    #[tokio::test]
    async fn semantic_search_recalls_chinese_by_paraphrase() {
        let c = ApiClient::new();
        let hits = c
            .semantic_search("qmd 索引因 node 升级坏了自动修复")
            .await
            .expect("search ok");
        assert!(!hits.is_empty(), "should recall something");
        assert!(
            hits.iter()
                .take(3)
                .any(|h| h.path.contains("qmd-self-healing")),
            "target page should be in top-3, got: {:?}",
            hits.iter().map(|h| &h.path).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn daemon_status_reachable_when_running() {
        let c = ApiClient::new();
        let st = c.daemon_status().await;
        assert!(st.reachable);
        assert!(st.page_count.unwrap_or(0) >= 40);
    }

    #[tokio::test]
    async fn write_and_delete_reject_traversal_paths_client_side() {
        let c = ApiClient::new();
        let mk = |p: &str| WritePageArgs {
            path: p.into(),
            body: "x".into(),
            title: None,
            kind: None,
            tier: None,
            tags: vec![],
            pinned: false,
            frontmatter: None,
        };
        assert!(c.write_page(&mk("../evil.md")).await.is_err());
        assert!(c.write_page(&mk("/abs/path.md")).await.is_err());
        assert!(c.delete_page("a/../b.md").await.is_err());
        assert!(c.delete_page("").await.is_err());
    }

    /// Scratch-daemon integration: set ENGRAM_TEST_BASE to a daemon with a
    /// throwaway data dir (never the real one — this test writes and
    /// deletes pages). Skips silently when unset.
    #[tokio::test]
    async fn phase2_roundtrip_on_scratch_daemon() {
        let Some(base) = std::env::var("ENGRAM_TEST_BASE").ok() else {
            eprintln!("skipped: ENGRAM_TEST_BASE not set");
            return;
        };
        let c = ApiClient::with_target(&base, "default", "scratch");

        // write → read back
        let args = WritePageArgs {
            path: "notes/desktop-e2e.md".into(),
            body: "# Desktop e2e\n\nwritten by the phase-2 roundtrip test".into(),
            title: None,
            kind: None,
            tier: None,
            tags: vec!["e2e".into()],
            pinned: false,
            frontmatter: None,
        };
        let res = c.write_page(&args).await.expect("write-page must succeed");
        assert_eq!(res.path, "notes/desktop-e2e.md");
        assert!(!res.page_id.is_empty());
        let detail = c
            .read_page("notes/desktop-e2e.md")
            .await
            .expect("read back");
        assert!(detail.body.contains("phase-2 roundtrip test"));
        assert!(
            !detail.body.starts_with("---"),
            "frontmatter must be stripped from the editable body: {:?}",
            detail.body
        );

        // admin status + health + sweep dry-run
        let st = c.admin_status().await.expect("admin status");
        assert!(st["version"].is_string(), "status must carry version: {st}");
        let health = c.memory_health().await.expect("memory health");
        let _ = health.stale + health.duplicates + health.orphans;
        let sweep = c.run_sweep(true).await.expect("sweep dry-run");
        assert!(sweep.is_object(), "sweep report must be an object: {sweep}");

        // embed without a configured provider must surface an error (503)
        let embed = c.run_embed(false, true).await;
        assert!(embed.is_err(), "scratch daemon has no embedder: {embed:?}");

        // backup returns a gzip tarball
        let bytes = c.backup().await.expect("backup bytes");
        assert!(
            bytes.len() > 2 && bytes[0] == 0x1f && bytes[1] == 0x8b,
            "gzip magic expected"
        );

        // delete → gone
        c.delete_page("notes/desktop-e2e.md")
            .await
            .expect("delete-page");
        assert!(
            c.read_page("notes/desktop-e2e.md").await.is_err(),
            "deleted page must 404"
        );
    }
}
