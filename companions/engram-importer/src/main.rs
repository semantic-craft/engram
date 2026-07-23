use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;
use walkdir::WalkDir;

const OMC_IMPORT_VERSION: &str = "omc-wiki-v1";
const OBSIDIAN_IMPORT_VERSION: &str = "obsidian-v1";
const DEFAULT_SERVER_URL: &str = "http://127.0.0.1:49374";

/// Timeout for the small metadata calls (project preflight, page list,
/// pre-write existence probe). These do no indexing work server-side,
/// so a slow one means a sick server, not a big page.
const METADATA_TIMEOUT_SECS: u64 = 30;

/// Default timeout for a single `POST /admin/write-page`.
///
/// The server embeds synchronously on the write path (deliberately —
/// no fire-and-forget indexing), and a long page embeds as one
/// provider call per markdown chunk, sequentially. A page at the
/// server's chunk cap is therefore dozens of provider round-trips
/// inside one request, and the embedding client itself allows 120s for
/// a single cold-model call. 30s — the old shared client timeout — is
/// under the cost of one cold chunk, let alone a capped page.
const DEFAULT_WRITE_TIMEOUT_SECS: u64 = 300;

/// The write budget must clear the embedding client's own 120s
/// single-call allowance, or a page whose first chunk hits a cold
/// model times out before the server has even started chunk two.
const _: () = assert!(DEFAULT_WRITE_TIMEOUT_SECS > 120);
const _: () = assert!(DEFAULT_WRITE_TIMEOUT_SECS > METADATA_TIMEOUT_SECS);

#[derive(Parser, Debug)]
#[command(author, version, about = "Optional engram import companion")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Import an oh-my-claudecode / OMC flat markdown wiki directory.
    OmcWiki(OmcArgs),
    /// Import an Obsidian folder tree (frontmatter passed through verbatim,
    /// short-name wikilinks rewritten to root-relative paths).
    Obsidian(ObsidianArgs),
}

#[derive(Parser, Debug, Clone)]
struct OmcArgs {
    #[arg(long)]
    dir: PathBuf,
    #[arg(long)]
    workspace: Option<String>,
    #[arg(long)]
    project: Option<String>,
    #[arg(long, env = "ENGRAM_SERVER_URL", default_value = DEFAULT_SERVER_URL)]
    server_url: String,
    #[arg(long)]
    apply: bool,
    #[arg(long)]
    manifest_out: Option<PathBuf>,
    #[arg(long)]
    create_destination: bool,
    #[arg(long)]
    overwrite: bool,
    #[arg(long)]
    include_session_logs: bool,
    #[arg(long)]
    show_body: bool,
    #[arg(long)]
    pinned: bool,
    /// Seconds to allow a single write-page request. Raise it for
    /// large pages against a slow embedding provider.
    #[arg(long, default_value_t = DEFAULT_WRITE_TIMEOUT_SECS)]
    write_timeout_secs: u64,
}

#[derive(Parser, Debug, Clone)]
struct ObsidianArgs {
    /// Source folder inside the vault (e.g. <vault>/Knowledge).
    #[arg(long)]
    dir: PathBuf,
    /// Destination path prefix inside the project wiki (e.g. `knowledge`).
    #[arg(long)]
    dest_prefix: String,
    #[arg(long)]
    workspace: Option<String>,
    #[arg(long)]
    project: Option<String>,
    #[arg(long, env = "ENGRAM_SERVER_URL", default_value = DEFAULT_SERVER_URL)]
    server_url: String,
    #[arg(long)]
    apply: bool,
    #[arg(long)]
    manifest_out: Option<PathBuf>,
    #[arg(long)]
    create_destination: bool,
    #[arg(long)]
    overwrite: bool,
    #[arg(long)]
    show_body: bool,
    #[arg(long)]
    pinned: bool,
    /// Extra tag applied to every imported page (repeatable).
    #[arg(long = "tag")]
    tags: Vec<String>,
    /// Seconds to allow a single write-page request. Raise it for
    /// large pages against a slow embedding provider.
    #[arg(long, default_value_t = DEFAULT_WRITE_TIMEOUT_SECS)]
    write_timeout_secs: u64,
}

#[derive(Debug, Clone)]
struct PlannedPage {
    source_path: String,
    source_sha256: String,
    destination_path: String,
    request: WritePageRequest,
}

#[derive(Debug, Serialize, Clone)]
struct ManifestEntry {
    import_version: String,
    source_path: String,
    source_sha256: String,
    destination_path: String,
    status: ManifestStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    page_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    checkpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum ManifestStatus {
    Planned,
    Written,
    /// The write request timed out, but the destination page was found
    /// on the server afterwards. The server commits the page row and
    /// the on-disk file *before* it embeds, so a write that times out
    /// during embedding has still landed — recording it as `Failed`
    /// would be a lie, and re-running the import would need
    /// `--overwrite` to get past the path it claims was never written.
    /// The page may be indexed without vectors; `engram embed` backfills.
    Uncertain,
    Failed,
}

#[derive(Debug, Serialize, Clone)]
struct Manifest {
    import_version: String,
    entries: Vec<ManifestEntry>,
}

#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
struct WritePageRequest {
    workspace: String,
    project: String,
    path: String,
    body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    tier: String,
    tags: Vec<String>,
    pinned: bool,
    /// Custom frontmatter keys passed through verbatim to the admin
    /// endpoint (which treats them as the authoritative base map).
    #[serde(skip_serializing_if = "Option::is_none")]
    frontmatter: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Debug, Deserialize)]
struct WritePageResponse {
    page_id: String,
    path: String,
    checkpoint: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PageListItem {
    path: String,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum PageListBody {
    Bare(Vec<PageListItem>),
    Wrapped { pages: Vec<PageListItem> },
}

impl PageListBody {
    fn into_pages(self) -> Vec<PageListItem> {
        match self {
            Self::Bare(pages) | Self::Wrapped { pages } => pages,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Commands::OmcWiki(args) => run_omc(args).await,
        Commands::Obsidian(args) => run_obsidian(args).await,
    }
}

/// Options shared by every import source's post-planning phase.
struct RunOpts<'a> {
    workspace: String,
    project: String,
    server_url: &'a str,
    apply: bool,
    manifest_out: Option<&'a Path>,
    create_destination: bool,
    overwrite: bool,
    show_body: bool,
    import_version: &'a str,
    write_timeout: Duration,
}

fn require_apply_args(
    apply: bool,
    workspace: Option<&str>,
    project: Option<&str>,
    manifest_out: Option<&Path>,
) -> Result<()> {
    if apply {
        if workspace.is_none_or(|s| s.trim().is_empty())
            || project.is_none_or(|s| s.trim().is_empty())
        {
            bail!("--apply requires explicit --workspace and --project");
        }
        if manifest_out.is_none() {
            bail!("--apply requires --manifest-out <path>");
        }
    }
    Ok(())
}

async fn run_omc(args: OmcArgs) -> Result<()> {
    require_apply_args(
        args.apply,
        args.workspace.as_deref(),
        args.project.as_deref(),
        args.manifest_out.as_deref(),
    )?;
    let workspace = args
        .workspace
        .clone()
        .unwrap_or_else(|| "DRY-RUN-WORKSPACE".into());
    let project = args
        .project
        .clone()
        .unwrap_or_else(|| "DRY-RUN-PROJECT".into());
    let planned = plan_omc_wiki(
        &args.dir,
        &workspace,
        &project,
        args.include_session_logs,
        args.pinned,
    )?;
    run_import(
        planned,
        RunOpts {
            workspace,
            project,
            server_url: &args.server_url,
            apply: args.apply,
            manifest_out: args.manifest_out.as_deref(),
            create_destination: args.create_destination,
            overwrite: args.overwrite,
            show_body: args.show_body,
            import_version: OMC_IMPORT_VERSION,
            write_timeout: Duration::from_secs(args.write_timeout_secs),
        },
    )
    .await
}

async fn run_obsidian(args: ObsidianArgs) -> Result<()> {
    require_apply_args(
        args.apply,
        args.workspace.as_deref(),
        args.project.as_deref(),
        args.manifest_out.as_deref(),
    )?;
    let workspace = args
        .workspace
        .clone()
        .unwrap_or_else(|| "DRY-RUN-WORKSPACE".into());
    let project = args
        .project
        .clone()
        .unwrap_or_else(|| "DRY-RUN-PROJECT".into());
    let planned = plan_obsidian(
        &args.dir,
        &args.dest_prefix,
        &workspace,
        &project,
        args.pinned,
        &args.tags,
    )?;
    run_import(
        planned,
        RunOpts {
            workspace,
            project,
            server_url: &args.server_url,
            apply: args.apply,
            manifest_out: args.manifest_out.as_deref(),
            create_destination: args.create_destination,
            overwrite: args.overwrite,
            show_body: args.show_body,
            import_version: OBSIDIAN_IMPORT_VERSION,
            write_timeout: Duration::from_secs(args.write_timeout_secs),
        },
    )
    .await
}

async fn run_import(planned: Vec<PlannedPage>, opts: RunOpts<'_>) -> Result<()> {
    let RunOpts {
        workspace,
        project,
        server_url,
        apply,
        manifest_out,
        create_destination,
        overwrite,
        show_body,
        import_version,
        write_timeout,
    } = opts;
    let mut entries: Vec<_> = planned
        .iter()
        .map(|p| planned_entry(p, import_version))
        .collect();

    if !apply {
        if show_body {
            for page in &planned {
                println!(
                    "--- {} -> {} ---\n{}",
                    page.source_path, page.destination_path, page.request.body
                );
            }
        }
        if let Some(path) = manifest_out {
            write_manifest(path, &entries, import_version)?;
        } else {
            println!(
                "dry-run: planned {} writes; no HTTP writes performed",
                planned.len()
            );
            for page in &planned {
                println!("{} -> {}", page.source_path, page.destination_path);
            }
        }
        return Ok(());
    }

    let manifest_path = manifest_out.unwrap();
    write_manifest(manifest_path, &entries, import_version)?;

    let client = ImportClient::new(server_url, write_timeout)?;
    let destination_exists = client
        .preflight_project(&workspace, &project, create_destination)
        .await?;
    if !overwrite && destination_exists {
        let existing = client.existing_paths(&workspace, &project).await?;
        let conflicts: Vec<_> = planned
            .iter()
            .filter(|p| existing.contains_key(&p.destination_path))
            .collect();
        if !conflicts.is_empty() {
            bail!(
                "destination already has {} planned path(s); rerun with --overwrite to replace",
                conflicts.len()
            );
        }
    }

    for (idx, page) in planned.iter().enumerate() {
        if !overwrite
            && client
                .page_exists(&workspace, &project, &page.destination_path)
                .await?
        {
            entries[idx].status = ManifestStatus::Failed;
            entries[idx].error = Some("destination page appeared before write; aborting".into());
            write_manifest(manifest_path, &entries, import_version)?;
            bail!(
                "destination page appeared before write {}; completed {} writes",
                page.destination_path,
                idx
            );
        }
        match client.write_page(&page.request).await {
            Ok(resp) => {
                entries[idx].status = ManifestStatus::Written;
                entries[idx].page_id = Some(resp.page_id);
                entries[idx].path = Some(resp.path);
                entries[idx].checkpoint = resp.checkpoint;
                write_manifest(manifest_path, &entries, import_version)?;
            }
            Err(err) if is_timeout(&err) => {
                // A timeout says nothing about whether the write
                // landed: the server commits the page row and the file
                // before it embeds, and embedding a long page is many
                // sequential provider calls inside the same request.
                // Ask the server what actually happened rather than
                // assuming the worst and aborting the rest of the run.
                match client
                    .page_exists(&workspace, &project, &page.destination_path)
                    .await
                {
                    Ok(true) => {
                        entries[idx].status = ManifestStatus::Uncertain;
                        entries[idx].error = Some(format!(
                            "write timed out ({err}); destination page is present on the server, \
                             so the write landed but returned no page id. Its embedding may be \
                             missing — run `engram embed` to backfill. Re-importing this page \
                             needs --overwrite."
                        ));
                        write_manifest(manifest_path, &entries, import_version)?;
                        eprintln!(
                            "warning: write timed out but page is present, continuing: {}",
                            page.destination_path
                        );
                    }
                    Ok(false) => {
                        entries[idx].status = ManifestStatus::Failed;
                        entries[idx].error = Some(format!(
                            "write timed out ({err}); destination page absent on the server, \
                             so the write did not land"
                        ));
                        write_manifest(manifest_path, &entries, import_version)?;
                        bail!("live write timed out after {} completed writes: {err}", idx);
                    }
                    Err(probe_err) => {
                        entries[idx].status = ManifestStatus::Uncertain;
                        entries[idx].error = Some(format!(
                            "write timed out ({err}) and the follow-up existence check failed \
                             ({probe_err}); verify this page on the server before re-importing"
                        ));
                        write_manifest(manifest_path, &entries, import_version)?;
                        bail!(
                            "live write timed out and could not be verified after {} completed \
                             writes: {err}",
                            idx
                        );
                    }
                }
            }
            Err(err) => {
                entries[idx].status = ManifestStatus::Failed;
                entries[idx].error = Some(err.to_string());
                write_manifest(manifest_path, &entries, import_version)?;
                bail!("live write failed after {} completed writes: {err}", idx);
            }
        }
    }
    let uncertain = entries
        .iter()
        .filter(|e| e.status == ManifestStatus::Uncertain)
        .count();
    println!(
        "import complete: wrote {} pages",
        entries
            .iter()
            .filter(|e| e.status == ManifestStatus::Written)
            .count()
    );
    if uncertain > 0 {
        println!(
            "{uncertain} page(s) timed out but were found on the server (status \"uncertain\" in \
             the manifest); their embeddings may be missing — run `engram embed` to backfill"
        );
    }
    Ok(())
}

fn planned_entry(page: &PlannedPage, import_version: &str) -> ManifestEntry {
    ManifestEntry {
        import_version: import_version.into(),
        source_path: page.source_path.clone(),
        source_sha256: page.source_sha256.clone(),
        destination_path: page.destination_path.clone(),
        status: ManifestStatus::Planned,
        page_id: None,
        path: None,
        checkpoint: None,
        error: None,
    }
}

fn write_manifest(path: &Path, entries: &[ManifestEntry], import_version: &str) -> Result<()> {
    let manifest = Manifest {
        import_version: import_version.into(),
        entries: entries.to_vec(),
    };
    atomic_write(path, serde_json::to_string_pretty(&manifest)?.as_bytes())
        .with_context(|| format!("write manifest {}", path.display()))
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        bail!(
            "manifest parent directory does not exist: {}",
            parent.display()
        );
    }
    let tmp = temp_path_for(path)?;
    fs::write(&tmp, bytes)?;
    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = fs::remove_file(&tmp);
            Err(err.into())
        }
    }
}

fn temp_path_for(path: &Path) -> Result<PathBuf> {
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow!("manifest path must include a file name"))?;
    Ok(path.with_file_name(format!(".{file_name}.tmp")))
}

fn plan_omc_wiki(
    dir: &Path,
    workspace: &str,
    project: &str,
    include_session_logs: bool,
    pinned: bool,
) -> Result<Vec<PlannedPage>> {
    let root =
        fs::canonicalize(dir).with_context(|| format!("read source dir {}", dir.display()))?;
    if !root.is_dir() {
        bail!("--dir must be a directory");
    }
    let mut planned = Vec::new();
    let mut destinations: HashMap<String, String> = HashMap::new();
    for entry in WalkDir::new(&root)
        .min_depth(1)
        .max_depth(1)
        .sort_by_file_name()
    {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let rel = path
            .strip_prefix(&root)?
            .to_string_lossy()
            .replace('\\', "/");
        validate_source_rel(&rel)?;
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow!("invalid filename"))?;
        if name == "index.md" || (!include_session_logs && name.starts_with("session-log-")) {
            continue;
        }
        let content =
            fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let source_sha256 = sha256_hex(content.as_bytes());
        let parsed = parse_markdown(&content)?;
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow!("invalid filename"))?;
        let dest = format!("omc/{}.md", slugify(stem));
        validate_destination_path(&dest)?;
        if let Some(other) = destinations.insert(dest.clone(), rel.clone()) {
            bail!("duplicate destination path collision: {other} and {rel} both map to {dest}");
        }
        planned.push(PlannedPage {
            source_path: rel,
            source_sha256,
            destination_path: dest.clone(),
            request: WritePageRequest {
                workspace: workspace.to_owned(),
                project: project.to_owned(),
                path: dest,
                body: parsed.body,
                title: parsed.title,
                kind: parsed.kind,
                tier: normalize_tier(parsed.tier.as_deref())?,
                tags: parsed.tags,
                pinned: parsed.pinned || pinned,
                frontmatter: None,
            },
        });
    }
    Ok(planned)
}

/// Plan an Obsidian folder import: recursive walk, original file names
/// preserved (no slugify — the wiki accepts non-ASCII path segments),
/// frontmatter passed through verbatim, short-name wikilinks rewritten
/// to root-relative destination paths where the stem match is unique.
fn plan_obsidian(
    dir: &Path,
    dest_prefix: &str,
    workspace: &str,
    project: &str,
    pinned: bool,
    extra_tags: &[String],
) -> Result<Vec<PlannedPage>> {
    let root =
        fs::canonicalize(dir).with_context(|| format!("read source dir {}", dir.display()))?;
    if !root.is_dir() {
        bail!("--dir must be a directory");
    }
    let dest_prefix = dest_prefix.trim_matches('/');
    if dest_prefix.is_empty() {
        bail!("--dest-prefix must not be empty");
    }
    let mut planned = Vec::new();
    for entry in WalkDir::new(&root).min_depth(1).sort_by_file_name() {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let rel = path
            .strip_prefix(&root)?
            .to_string_lossy()
            .replace('\\', "/");
        validate_source_rel(&rel)?;
        // Skip Obsidian infrastructure: hidden dirs (.obsidian, .trash)
        // and underscore-prefixed files/dirs (_index.md, _Templates).
        if rel
            .split('/')
            .any(|seg| seg.starts_with('.') || seg.starts_with('_'))
        {
            continue;
        }
        let content =
            fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let source_sha256 = sha256_hex(content.as_bytes());
        let parsed = parse_markdown(&content)?;
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow!("invalid filename"))?;
        let dest = format!("{dest_prefix}/{rel}");
        validate_destination_path(&dest)?;
        let mut tags = parsed.tags;
        for t in extra_tags {
            if !tags.contains(t) {
                tags.push(t.clone());
            }
        }
        planned.push(PlannedPage {
            source_path: rel,
            source_sha256,
            destination_path: dest.clone(),
            request: WritePageRequest {
                workspace: workspace.to_owned(),
                project: project.to_owned(),
                path: dest,
                body: parsed.body,
                title: parsed.title.or_else(|| Some(stem.to_owned())),
                kind: parsed.kind,
                tier: normalize_tier(parsed.tier.as_deref())?,
                tags,
                pinned: parsed.pinned || pinned,
                frontmatter: if parsed.extra.is_empty() {
                    None
                } else {
                    Some(parsed.extra)
                },
            },
        });
    }
    rewrite_wikilinks(&mut planned);
    Ok(planned)
}

/// Rewrite Obsidian short-name wikilinks (`[[name]]`, `[[name|label]]`)
/// to root-relative destination paths when exactly one planned page's
/// file stem matches. Ambiguous or unresolved names are left verbatim
/// (engram stores them as unresolved forward links) with a warning.
fn rewrite_wikilinks(planned: &mut [PlannedPage]) {
    let mut stems: HashMap<String, Vec<String>> = HashMap::new();
    for page in planned.iter() {
        let stem = Path::new(&page.destination_path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_owned();
        let target = page
            .destination_path
            .strip_suffix(".md")
            .unwrap_or(&page.destination_path)
            .to_owned();
        stems.entry(stem).or_default().push(target);
    }
    for page in planned.iter_mut() {
        let (body, warnings) = rewrite_body_wikilinks(&page.request.body, &stems);
        for w in warnings {
            eprintln!("warning: {}: {w}", page.source_path);
        }
        page.request.body = body;
    }
}

fn rewrite_body_wikilinks(
    body: &str,
    stems: &HashMap<String, Vec<String>>,
) -> (String, Vec<String>) {
    let mut out = String::with_capacity(body.len());
    let mut warnings = Vec::new();
    let mut in_fence = false;
    for (i, line) in body.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            out.push_str(line);
            continue;
        }
        if in_fence {
            out.push_str(line);
            continue;
        }
        out.push_str(&rewrite_line_wikilinks(line, stems, &mut warnings));
    }
    (out, warnings)
}

fn rewrite_line_wikilinks(
    line: &str,
    stems: &HashMap<String, Vec<String>>,
    warnings: &mut Vec<String>,
) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(start) = rest.find("[[") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("]]") else {
            break;
        };
        let raw = &after[..end];
        out.push_str(&rest[..start]);
        let (target, label) = match raw.split_once('|') {
            Some((t, l)) => (t.trim(), Some(l.trim())),
            None => (raw.trim(), None),
        };
        // Only bare short names are rewritten: anything already pathy
        // (`/`), scoped (`:`), embedded (`![[..]]` handled upstream as
        // plain text), or an anchor is left untouched.
        if target.contains('/') || target.contains(':') || target.contains('#') {
            out.push_str("[[");
            out.push_str(raw);
            out.push_str("]]");
        } else {
            match stems.get(target).map(Vec::as_slice) {
                Some([unique]) => {
                    out.push_str("[[");
                    out.push_str(unique);
                    out.push('|');
                    out.push_str(label.unwrap_or(target));
                    out.push_str("]]");
                }
                Some(multi) => {
                    warnings.push(format!(
                        "ambiguous wikilink [[{target}]] ({} candidates); left verbatim",
                        multi.len()
                    ));
                    out.push_str("[[");
                    out.push_str(raw);
                    out.push_str("]]");
                }
                None => {
                    warnings.push(format!("unresolved wikilink [[{target}]]; left verbatim"));
                    out.push_str("[[");
                    out.push_str(raw);
                    out.push_str("]]");
                }
            }
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

fn normalize_tier(tier: Option<&str>) -> Result<String> {
    let tier = tier.unwrap_or("semantic").trim();
    match tier {
        "working" | "episodic" | "semantic" | "procedural" => Ok(tier.to_owned()),
        other => bail!("unsupported tier '{other}'"),
    }
}

#[derive(Default)]
struct ParsedMarkdown {
    body: String,
    title: Option<String>,
    kind: Option<String>,
    tier: Option<String>,
    tags: Vec<String>,
    pinned: bool,
    /// Frontmatter keys not mapped to a dedicated request field,
    /// preserved verbatim for endpoint passthrough.
    extra: serde_json::Map<String, serde_json::Value>,
}

fn parse_markdown(input: &str) -> Result<ParsedMarkdown> {
    let mut out = ParsedMarkdown::default();
    let body = if let Some(rest) = input.strip_prefix("---\n") {
        if let Some(end) = rest.find("\n---\n") {
            let yaml = &rest[..end];
            let value: serde_yaml::Value =
                serde_yaml::from_str(yaml).context("parse YAML frontmatter")?;
            if let Some(map) = value.as_mapping() {
                out.title = yaml_string(map, "title");
                out.kind = yaml_string(map, "kind");
                out.tier = yaml_string(map, "tier");
                out.pinned = yaml_bool(map, "pinned").unwrap_or(false);
                out.tags = yaml_tags(map);
                for (k, v) in map {
                    let Some(key) = k.as_str() else { continue };
                    if matches!(key, "title" | "kind" | "tier" | "tags" | "pinned") {
                        continue;
                    }
                    let json = serde_json::to_value(v)
                        .with_context(|| format!("frontmatter key '{key}' not JSON-mappable"))?;
                    out.extra.insert(key.to_owned(), json);
                }
            }
            rest[end + "\n---\n".len()..].to_owned()
        } else {
            input.to_owned()
        }
    } else {
        input.to_owned()
    };
    if out.title.is_none() {
        out.title = first_h1(&body);
    }
    out.body = body;
    Ok(out)
}

fn yaml_key(key: &str) -> serde_yaml::Value {
    serde_yaml::Value::String(key.into())
}
fn yaml_string(map: &serde_yaml::Mapping, key: &str) -> Option<String> {
    map.get(yaml_key(key))
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .filter(|s| !s.trim().is_empty())
}
fn yaml_bool(map: &serde_yaml::Mapping, key: &str) -> Option<bool> {
    map.get(yaml_key(key)).and_then(|v| v.as_bool())
}
fn yaml_tags(map: &serde_yaml::Mapping) -> Vec<String> {
    match map.get(yaml_key("tags")) {
        Some(serde_yaml::Value::Sequence(seq)) => seq
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
        Some(v) => v.as_str().map(|s| vec![s.to_owned()]).unwrap_or_default(),
        None => Vec::new(),
    }
}
fn first_h1(body: &str) -> Option<String> {
    body.lines()
        .find_map(|l| l.strip_prefix("# ").map(str::trim).map(str::to_owned))
        .filter(|s| !s.is_empty())
}

fn validate_source_rel(rel: &str) -> Result<()> {
    let p = Path::new(rel);
    if p.is_absolute() || p.components().any(|c| matches!(c, Component::ParentDir)) {
        bail!("unsafe source relative path: {rel}");
    }
    Ok(())
}

fn validate_destination_path(path: &str) -> Result<()> {
    let p = Path::new(path);
    if p.is_absolute()
        || path.starts_with('/')
        || path.contains('\\')
        || p.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        bail!("unsafe destination path: {path}");
    }
    let first = path.split('/').next().unwrap_or_default();
    if matches!(
        first,
        "_rules"
            | "_internal"
            | ".git"
            | "sessions"
            | "session-logs"
            | "procedures"
            | "decisions"
            | "gotchas"
    ) {
        bail!("reserved destination prefix: {first}");
    }
    if !path.ends_with(".md") {
        bail!("destination path must end in .md");
    }
    Ok(())
}

fn slugify(s: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for ch in s.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            dash = false;
        } else if !dash {
            out.push('-');
            dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "page".into()
    } else {
        trimmed
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

struct ImportClient {
    client: reqwest::Client,
    origin: String,
    base_path: String,
    token: Option<String>,
    write_timeout: Duration,
}

impl ImportClient {
    fn new(server_url: &str, write_timeout: Duration) -> Result<Self> {
        let url = Url::parse(server_url).context("invalid --server-url")?;
        let origin = format!(
            "{}://{}",
            url.scheme(),
            url.host_str()
                .ok_or_else(|| anyhow!("server URL needs host"))?
        );
        let origin = if let Some(port) = url.port() {
            format!("{origin}:{port}")
        } else {
            origin
        };
        let base_path = url.path().trim_end_matches('/').to_owned();
        let base_path = if base_path == "/" {
            String::new()
        } else {
            base_path
        };
        Ok(Self {
            // Metadata calls keep the short timeout; write-page
            // overrides it per request (see `write_page`).
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(METADATA_TIMEOUT_SECS))
                .build()?,
            origin,
            base_path,
            token: std::env::var("ENGRAM_AUTH_TOKEN")
                .ok()
                .filter(|s| !s.is_empty()),
            write_timeout,
        })
    }
    fn url(&self, path: &str) -> String {
        format!("{}{}{}", self.origin, self.base_path, path)
    }
    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let req = self.client.request(method, self.url(path));
        if let Some(token) = &self.token {
            req.bearer_auth(token)
        } else {
            req
        }
    }
    async fn preflight_project(
        &self,
        workspace: &str,
        project: &str,
        create: bool,
    ) -> Result<bool> {
        let resp = self
            .request(
                reqwest::Method::GET,
                &format!(
                    "/api/v1/workspaces/{}/projects/{}/pages",
                    enc(workspace),
                    enc(project)
                ),
            )
            .send()
            .await?;
        if resp.status().is_success() {
            Ok(true)
        } else if resp.status() == StatusCode::NOT_FOUND && create {
            Ok(false)
        } else {
            bail!(
                "destination workspace/project must already exist (or pass --create-destination): HTTP {}",
                resp.status()
            )
        }
    }
    async fn existing_paths(&self, workspace: &str, project: &str) -> Result<HashMap<String, ()>> {
        let resp = self
            .request(
                reqwest::Method::GET,
                &format!(
                    "/api/v1/workspaces/{}/projects/{}/pages",
                    enc(workspace),
                    enc(project)
                ),
            )
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("list destination pages failed: HTTP {}", resp.status());
        }
        let pages: PageListBody = resp.json().await?;
        Ok(pages
            .into_pages()
            .into_iter()
            .map(|page| (page.path, ()))
            .collect())
    }
    async fn page_exists(&self, workspace: &str, project: &str, path: &str) -> Result<bool> {
        let resp = self
            .request(
                reqwest::Method::GET,
                &format!(
                    "/api/v1/workspaces/{}/projects/{}/pages/{}",
                    enc(workspace),
                    enc(project),
                    enc_path(path)
                ),
            )
            .send()
            .await?;
        match resp.status() {
            StatusCode::OK => Ok(true),
            StatusCode::NOT_FOUND => Ok(false),
            s => bail!("page pre-write check failed: HTTP {s}"),
        }
    }
    async fn write_page(&self, req: &WritePageRequest) -> Result<WritePageResponse> {
        let resp = self
            .request(reqwest::Method::POST, "/admin/write-page")
            .timeout(self.write_timeout)
            .json(req)
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("write-page failed: HTTP {}", resp.status());
        }
        Ok(resp.json().await?)
    }
}

/// `true` if the failure was a client-side timeout rather than a
/// refusal by the server. A timeout carries no information about
/// whether the write landed; every other error here (connect refused,
/// non-2xx status, malformed response) means the server either never
/// took the request or explicitly rejected it.
fn is_timeout(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(reqwest::Error::is_timeout)
    })
}

fn enc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}
fn enc_path(s: &str) -> String {
    s.split('/').map(enc).collect::<Vec<_>>().join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write(dir: &Path, name: &str, body: &str) {
        fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn parses_omc_frontmatter() {
        let parsed = parse_markdown("---\ntitle: T\nkind: rule\ntier: procedural\ntags: [a, b]\npinned: true\nextra: ignored\n---\n# Body\ntext").unwrap();
        assert_eq!(parsed.title.as_deref(), Some("T"));
        assert_eq!(parsed.kind.as_deref(), Some("rule"));
        assert_eq!(parsed.tier.as_deref(), Some("procedural"));
        assert_eq!(parsed.tags, vec!["a", "b"]);
        assert!(parsed.pinned);
        assert_eq!(parsed.body, "# Body\ntext");
    }

    #[test]
    fn planning_rejects_unknown_tier_before_live_write() {
        let td = tempdir().unwrap();
        write(td.path(), "note.md", "---\ntier: legendary\n---\n# Note");
        assert!(
            plan_omc_wiki(td.path(), "w", "p", false, false)
                .unwrap_err()
                .to_string()
                .contains("unsupported tier")
        );
    }

    #[test]
    fn skips_index_and_session_logs_by_default() {
        let td = tempdir().unwrap();
        write(td.path(), "index.md", "# Index");
        write(td.path(), "session-log-1.md", "# Log");
        write(td.path(), "note.md", "# Note");
        let pages = plan_omc_wiki(td.path(), "w", "p", false, false).unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].destination_path, "omc/note.md");
    }

    #[test]
    fn path_traversal_rejected() {
        assert!(validate_source_rel("../x.md").is_err());
        assert!(validate_destination_path("omc/../x.md").is_err());
        assert!(validate_destination_path("/omc/x.md").is_err());
    }

    #[test]
    fn detects_duplicate_slug_collisions() {
        let td = tempdir().unwrap();
        write(td.path(), "A B.md", "# A");
        write(td.path(), "a-b.md", "# B");
        assert!(
            plan_omc_wiki(td.path(), "w", "p", false, false)
                .unwrap_err()
                .to_string()
                .contains("collision")
        );
    }

    #[test]
    fn dry_run_planning_has_no_http_client() {
        let td = tempdir().unwrap();
        write(td.path(), "note.md", "# Note");
        let pages = plan_omc_wiki(td.path(), "w", "p", false, false).unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].request.workspace, "w");
    }

    #[test]
    fn write_request_json_shape() {
        let req = WritePageRequest {
            workspace: "w".into(),
            project: "p".into(),
            path: "omc/n.md".into(),
            body: "# N".into(),
            title: Some("N".into()),
            kind: Some("fact".into()),
            tier: "semantic".into(),
            tags: vec!["omc".into()],
            pinned: true,
            frontmatter: None,
        };
        let v = serde_json::to_value(req).unwrap();
        assert_eq!(v["workspace"], "w");
        assert_eq!(v["project"], "p");
        assert_eq!(v["path"], "omc/n.md");
        assert_eq!(v["body"], "# N");
        assert_eq!(v["pinned"], true);
        assert!(!v.as_object().unwrap().contains_key("frontmatter"));
    }

    #[test]
    fn parses_extra_frontmatter_for_passthrough() {
        let parsed = parse_markdown(
            "---\ntitle: T\ntype: scholar\naliases: [孔祥俊, Kong Xiangjun]\ncreated: '2026-04-06'\ntags: [a]\n---\n# Body",
        )
        .unwrap();
        assert_eq!(parsed.extra["type"], "scholar");
        assert_eq!(parsed.extra["aliases"][0], "孔祥俊");
        assert_eq!(parsed.extra["created"], "2026-04-06");
        assert!(!parsed.extra.contains_key("title"));
        assert!(!parsed.extra.contains_key("tags"));
    }

    fn stem_map(pairs: &[(&str, &[&str])]) -> HashMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.iter().map(|s| (*s).to_owned()).collect()))
            .collect()
    }

    #[test]
    fn rewrites_unique_shortname_wikilinks() {
        let stems = stem_map(&[("孔祥俊", &["knowledge/scholars/孔祥俊"])]);
        let (body, warnings) = rewrite_body_wikilinks("见 [[孔祥俊]] 的分析", &stems);
        assert_eq!(body, "见 [[knowledge/scholars/孔祥俊|孔祥俊]] 的分析");
        assert!(warnings.is_empty());
    }

    #[test]
    fn preserves_labels_when_rewriting() {
        let stems = stem_map(&[(
            "2026-05-02 身体图式",
            &["knowledge/concepts/Book-Brief/2026-05-02 身体图式"],
        )]);
        let (body, _) = rewrite_body_wikilinks("[[2026-05-02 身体图式|身体图式]]", &stems);
        assert_eq!(
            body,
            "[[knowledge/concepts/Book-Brief/2026-05-02 身体图式|身体图式]]"
        );
    }

    #[test]
    fn ambiguous_and_unresolved_wikilinks_left_verbatim() {
        let stems = stem_map(&[(
            "孔祥俊",
            &[
                "knowledge/scholars/孔祥俊",
                "knowledge/writing-craft/学者/孔祥俊",
            ],
        )]);
        let (body, warnings) = rewrite_body_wikilinks("[[孔祥俊]] 和 [[不存在]]", &stems);
        assert_eq!(body, "[[孔祥俊]] 和 [[不存在]]");
        assert_eq!(warnings.len(), 2);
        assert!(warnings[0].contains("ambiguous"));
        assert!(warnings[1].contains("unresolved"));
    }

    #[test]
    fn pathy_scoped_and_fenced_wikilinks_untouched() {
        let stems = stem_map(&[("note", &["knowledge/note"])]);
        let (body, warnings) = rewrite_body_wikilinks(
            "[[a/b]] [[proj:note]]\n```\n[[note]]\n```\n[[note]]",
            &stems,
        );
        assert_eq!(
            body,
            "[[a/b]] [[proj:note]]\n```\n[[note]]\n```\n[[knowledge/note|note]]"
        );
        assert!(warnings.is_empty());
    }

    #[test]
    fn plan_obsidian_maps_nested_paths_and_skips_underscored() {
        let td = tempdir().unwrap();
        fs::create_dir_all(td.path().join("concepts")).unwrap();
        write(
            &td.path().join("concepts"),
            "AI专利充分公开.md",
            "---\ntype: concept\ntags: [专利]\n---\n# AI专利充分公开\n正文",
        );
        write(td.path(), "_index.md", "# Index");
        let pages = plan_obsidian(
            td.path(),
            "knowledge",
            "w",
            "p",
            true,
            &["academic".to_owned()],
        )
        .unwrap();
        assert_eq!(pages.len(), 1);
        let req = &pages[0].request;
        assert_eq!(req.path, "knowledge/concepts/AI专利充分公开.md");
        assert_eq!(req.title.as_deref(), Some("AI专利充分公开"));
        assert!(req.pinned);
        assert_eq!(req.tags, vec!["专利", "academic"]);
        assert_eq!(req.frontmatter.as_ref().unwrap()["type"], "concept");
    }

    #[test]
    fn plan_obsidian_title_falls_back_to_stem() {
        let td = tempdir().unwrap();
        write(
            td.path(),
            "Balganesh_2026_Eunomics.md",
            "135 Yale Law Journal\n\nbody",
        );
        let pages = plan_obsidian(td.path(), "sources", "w", "p", false, &[]).unwrap();
        assert_eq!(
            pages[0].request.title.as_deref(),
            Some("Balganesh_2026_Eunomics")
        );
        assert!(pages[0].request.frontmatter.is_none());
    }

    #[test]
    fn parses_current_bare_api_page_list_shape() {
        let body = r#"[{"path":"omc/a.md"},{"path":"notes/b.md"}]"#;
        let parsed: PageListBody = serde_json::from_str(body).unwrap();
        let paths: Vec<_> = parsed.into_pages().into_iter().map(|p| p.path).collect();
        assert_eq!(paths, vec!["omc/a.md", "notes/b.md"]);
    }

    #[test]
    fn parses_legacy_wrapped_api_page_list_shape() {
        let body = r#"{"pages":[{"path":"omc/a.md"},{"path":"notes/b.md"}]}"#;
        let parsed: PageListBody = serde_json::from_str(body).unwrap();
        let paths: Vec<_> = parsed.into_pages().into_iter().map(|p| p.path).collect();
        assert_eq!(paths, vec!["omc/a.md", "notes/b.md"]);
    }

    #[test]
    fn auth_header_uses_bearer_scheme() {
        let rb = reqwest::Client::new()
            .get("http://127.0.0.1/")
            .bearer_auth("secret-token");
        let req = rb.build().unwrap();
        assert_eq!(
            req.headers().get(reqwest::header::AUTHORIZATION).unwrap(),
            "Bearer secret-token"
        );
        let dbg = format!("{:?}", req.headers());
        assert!(!dbg.contains("ENGRAM_AUTH_TOKEN"));
    }

    #[test]
    fn overwrite_requires_explicit_flag() {
        let args = OmcArgs {
            dir: PathBuf::from("."),
            workspace: Some("w".into()),
            project: Some("p".into()),
            server_url: DEFAULT_SERVER_URL.into(),
            apply: true,
            manifest_out: Some(PathBuf::from("m.json")),
            create_destination: false,
            overwrite: false,
            include_session_logs: false,
            show_body: false,
            pinned: false,
            write_timeout_secs: DEFAULT_WRITE_TIMEOUT_SECS,
        };
        assert!(!args.overwrite);
    }

    fn planned(dest: &str) -> PlannedPage {
        PlannedPage {
            source_path: format!("src/{dest}"),
            source_sha256: sha256_hex(dest.as_bytes()),
            destination_path: dest.to_string(),
            request: WritePageRequest {
                workspace: "ws".into(),
                project: "proj".into(),
                path: dest.to_string(),
                body: "body".into(),
                title: None,
                kind: None,
                tier: "semantic".into(),
                tags: vec![],
                pinned: false,
                frontmatter: None,
            },
        }
    }

    fn run_opts<'a>(
        server_url: &'a str,
        manifest: &'a Path,
        write_timeout: Duration,
    ) -> RunOpts<'a> {
        RunOpts {
            workspace: "ws".into(),
            project: "proj".into(),
            server_url,
            apply: true,
            manifest_out: Some(manifest),
            create_destination: false,
            // Skips the pre-write existence probe, so the only single-page
            // GET in the run is the post-timeout verification.
            overwrite: true,
            show_body: false,
            import_version: OBSIDIAN_IMPORT_VERSION,
            write_timeout,
        }
    }

    fn manifest_entries(path: &Path) -> Vec<serde_json::Value> {
        let raw = fs::read_to_string(path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        parsed["entries"].as_array().unwrap().clone()
    }

    /// Regression: a write that times out while the server is still
    /// embedding must not be recorded as `Failed`, and must not abort
    /// the rest of the import.
    ///
    /// `Wiki::write_page` commits the page row and the on-disk file
    /// *before* it awaits the embedding, and embedding a long page is
    /// one provider call per markdown chunk, sequentially, inside the
    /// request. So a timeout here means the page very likely landed.
    /// The old code marked it `Failed` and bailed, leaving a partially
    /// imported vault behind a manifest that said the page was never
    /// written — and a re-run then needed `--overwrite` to get past the
    /// path it claimed did not exist.
    #[tokio::test]
    async fn write_timeout_with_page_present_is_uncertain_and_import_continues() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        // Project preflight.
        Mock::given(method("GET"))
            .and(path("/api/v1/workspaces/ws/projects/proj/pages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&server)
            .await;

        // First write hangs past the client's budget; the second answers
        // normally. Registered first with `up_to_n_times(1)` so it is
        // consumed by page one only.
        Mock::given(method("POST"))
            .and(path("/admin/write-page"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(30)))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/admin/write-page"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "page_id": "page-2",
                "path": "b.md",
            })))
            .mount(&server)
            .await;

        // The post-timeout verification finds the page on the server.
        Mock::given(method("GET"))
            .and(path("/api/v1/workspaces/ws/projects/proj/pages/a.md"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "path": "a.md",
                "body_markdown": "body",
            })))
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let manifest = dir.path().join("manifest.json");

        let result = run_import(
            vec![planned("a.md"), planned("b.md")],
            run_opts(&server.uri(), &manifest, Duration::from_millis(300)),
        )
        .await;

        // The run completed rather than bailing at the first page.
        assert!(result.is_ok(), "import should continue: {result:?}");

        let entries = manifest_entries(&manifest);
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0]["status"], "uncertain",
            "a timed-out write whose page is present must not be recorded as failed"
        );
        assert!(
            entries[0]["error"]
                .as_str()
                .unwrap()
                .contains("present on the server"),
            "manifest must explain what is uncertain: {:?}",
            entries[0]["error"]
        );
        // The remaining page was still imported.
        assert_eq!(entries[1]["status"], "written");
        assert_eq!(entries[1]["page_id"], "page-2");
    }

    /// The timeout path must not swallow real failures: a server that
    /// rejects the write still aborts the run with `Failed`.
    #[tokio::test]
    async fn write_rejection_still_fails_and_aborts() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/workspaces/ws/projects/proj/pages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/admin/write-page"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let manifest = dir.path().join("manifest.json");

        let result = run_import(
            vec![planned("a.md"), planned("b.md")],
            run_opts(&server.uri(), &manifest, Duration::from_secs(5)),
        )
        .await;

        assert!(result.is_err(), "an explicit server rejection must abort");

        let entries = manifest_entries(&manifest);
        assert_eq!(entries[0]["status"], "failed");
        assert_eq!(
            entries[1]["status"], "planned",
            "the run must stop rather than continue past a real failure"
        );
    }

    /// A timeout whose page is *absent* afterwards is a genuine failure:
    /// the write did not land, so `Failed` + abort is still correct.
    #[tokio::test]
    async fn write_timeout_with_page_absent_still_fails() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/workspaces/ws/projects/proj/pages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/admin/write-page"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(30)))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/workspaces/ws/projects/proj/pages/a.md"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let manifest = dir.path().join("manifest.json");

        let result = run_import(
            vec![planned("a.md")],
            run_opts(&server.uri(), &manifest, Duration::from_millis(300)),
        )
        .await;

        assert!(result.is_err());
        let entries = manifest_entries(&manifest);
        assert_eq!(entries[0]["status"], "failed");
        assert!(
            entries[0]["error"]
                .as_str()
                .unwrap()
                .contains("did not land"),
            "{:?}",
            entries[0]["error"]
        );
    }
}
