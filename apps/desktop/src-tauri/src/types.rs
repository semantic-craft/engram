use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PageSummary {
    pub path: String,
    pub title: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LinkRef {
    pub path: String,
    pub title: String,
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PageDetail {
    pub path: String,
    pub title: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    // Accept the daemon's `body_markdown` on the way in, but serialize to
    // the frontend as `body` (a two-way `rename` would leak
    // `body_markdown` into the JS payload).
    #[serde(alias = "body_markdown")]
    pub body: String,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub frontmatter: serde_json::Value,
    #[serde(default)]
    pub links: Vec<LinkRef>,
    #[serde(default)]
    pub backlinks: Vec<LinkRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Hit {
    pub path: String,
    pub title: String,
    #[serde(default)]
    pub snippet: Option<String>,
    #[serde(default)]
    pub rank: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DaemonStatus {
    pub reachable: bool,
    #[serde(default)]
    pub page_count: Option<u64>,
}

/// Payload for `/admin/write-page`. Optional fields are omitted from the
/// request so the server's own defaults/derivation apply.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WritePageArgs {
    pub path: String,
    pub body: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub pinned: bool,
    /// Full frontmatter passthrough for `/admin/write-page`: when
    /// present it is the authoritative base (keys omitted here are
    /// dropped from the page); when absent the server merges with the
    /// existing page's frontmatter. The editor sends the frontmatter it
    /// read so custom keys survive and deliberate deletions stick.
    #[serde(default)]
    pub frontmatter: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WritePageResult {
    pub page_id: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HealthPageRef {
    pub path: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub kind: Option<String>,
}

/// The `health` block of the project overview response.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MemoryHealth {
    #[serde(default)]
    pub stale: u64,
    #[serde(default)]
    pub duplicates: u64,
    #[serde(default)]
    pub orphans: u64,
    #[serde(default)]
    pub stale_pages: Vec<HealthPageRef>,
    #[serde(default)]
    pub duplicate_pages: Vec<HealthPageRef>,
    #[serde(default)]
    pub orphan_pages: Vec<HealthPageRef>,
}

/// Summary response from `POST /admin/embed`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct EmbedReport {
    #[serde(default)]
    pub embedded: u64,
    #[serde(default)]
    pub skipped: u64,
    #[serde(default)]
    pub failed: u64,
    #[serde(default)]
    pub would_embed: u64,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub dim: u32,
}
