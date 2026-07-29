//! Deterministic, read-only project instruction inventory.
//!
//! This module deliberately does not depend on runtime [`crate::config::Config`],
//! an LLM provider, the Wiki, or the store. `instructions doctor` is dispatched
//! before configuration and tracing initialization, so inspecting a repository
//! cannot create logs, indexes, proposals, audit rows, or Wiki state.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use engram_core::{MARKER_END, MARKER_START, SNIPPET_BODY};
use serde::Serialize;
use toml_edit::DocumentMut;

use crate::instruction_placement::{
    PlacementChain, PlacementChainEntry, PlacementFinding, PlacementSource,
};

const DEFAULT_CODEX_PROJECT_DOC_MAX_BYTES: usize = 32 * 1024;
const NEAR_DUPLICATE_THRESHOLD: f64 = 0.8;
const CLAUDE_IMPORT_MAX_DEPTH: usize = 5;
const CLAUDE_PROJECT_SOURCES: [&str; 2] = ["CLAUDE.md", ".claude/CLAUDE.md"];

#[derive(Debug, Serialize)]
pub struct DoctorReport {
    pub schema_version: u32,
    pub read_only: bool,
    pub repository_root: PathBuf,
    pub working_directory: PathBuf,
    pub token_estimate: &'static str,
    pub canonical: CanonicalSource,
    pub sources: Vec<SourceReport>,
    pub chains: Vec<ChainReport>,
    pub placement_findings: Vec<PlacementFinding>,
    pub findings: Vec<Finding>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CanonicalSource {
    pub path: Option<String>,
    pub basis: String,
    pub candidates: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceReport {
    pub path: String,
    pub scope: String,
    pub line_count: usize,
    pub byte_count: usize,
    pub estimated_tokens: usize,
    pub marker_health: MarkerHealth,
    pub routing_asset_drift: RoutingAssetDrift,
    pub symlink: Option<SymlinkReport>,
    pub read_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SymlinkReport {
    pub target: String,
    pub safe: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct MarkerHealth {
    pub status: String,
    pub start_count: usize,
    pub end_count: usize,
    pub issues: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RoutingAssetDrift {
    pub status: String,
    pub drifted: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct ChainReport {
    pub harness: String,
    pub support: String,
    pub total_loaded_bytes: usize,
    pub project_document_max_bytes: Option<usize>,
    pub entries: Vec<ChainEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChainEntry {
    pub order: Option<usize>,
    pub source: String,
    pub classification: String,
    pub load_mode: String,
    pub effective: bool,
    pub reason: String,
    pub loaded_bytes: usize,
    pub truncated: bool,
    pub path_patterns: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub severity: String,
    pub code: String,
    pub message: String,
    pub sources: Vec<String>,
}

#[derive(Debug)]
struct SourceData {
    report: SourceReport,
    absolute_path: PathBuf,
    content: Option<String>,
}

#[derive(Debug)]
struct Inventory {
    repository_root: PathBuf,
    sources: BTreeMap<String, SourceData>,
    findings: Vec<Finding>,
}

#[derive(Debug)]
struct CanonicalDecision {
    report: CanonicalSource,
    adapters: BTreeSet<String>,
}

#[derive(Debug)]
struct CodexSettings {
    home: Option<PathBuf>,
    fallback_filenames: Vec<String>,
    project_doc_max_bytes: usize,
}

impl DoctorReport {
    pub fn inspect_current_repository() -> Result<Self> {
        let working_directory = std::env::current_dir().context("getting current directory")?;
        let repository_root = discover_repository_root(&working_directory)
            .unwrap_or_else(|| working_directory.clone());
        let mut inventory = Inventory::new(repository_root.clone());

        for relative in ["CLAUDE.md", ".claude/CLAUDE.md", "AGENTS.md"] {
            let path = repository_root.join(relative);
            inventory.ensure_source(&path, "project");
        }

        let canonical = detect_canonical(&mut inventory);
        let mut chains = vec![
            build_claude_chain(&mut inventory, &working_directory, &canonical),
            build_codex_chain(&mut inventory, &working_directory, &canonical),
        ];
        chains.extend(build_best_effort_chains(&mut inventory));

        let placement_sources = inventory
            .sources
            .values()
            .filter_map(|source| {
                source.content.as_ref().map(|content| PlacementSource {
                    path: source.report.path.clone(),
                    absolute_path: source.absolute_path.clone(),
                    content: content.clone(),
                    line_count: source.report.line_count,
                    safe_symlink_target: source
                        .report
                        .symlink
                        .as_ref()
                        .filter(|symlink| symlink.safe)
                        .map(|symlink| symlink.target.clone()),
                })
            })
            .collect();
        let placement_chains = chains
            .iter()
            .map(|chain| PlacementChain {
                harness: chain.harness.clone(),
                total_loaded_bytes: chain.total_loaded_bytes,
                project_document_max_bytes: chain.project_document_max_bytes,
                entries: chain
                    .entries
                    .iter()
                    .map(|entry| PlacementChainEntry {
                        source: entry.source.clone(),
                        load_mode: entry.load_mode.clone(),
                        effective: entry.effective,
                        loaded_bytes: entry.loaded_bytes,
                        truncated: entry.truncated,
                    })
                    .collect(),
            })
            .collect();
        let placement_findings = crate::instruction_placement::analyze(
            &repository_root,
            placement_sources,
            placement_chains,
        );

        let mut sources: Vec<_> = inventory
            .sources
            .into_values()
            .map(|source| source.report)
            .collect();
        sources.sort_by(|left, right| left.path.cmp(&right.path));
        inventory.findings.sort_by(|left, right| {
            (&left.severity, &left.code, &left.sources).cmp(&(
                &right.severity,
                &right.code,
                &right.sources,
            ))
        });

        Ok(Self {
            schema_version: 2,
            read_only: true,
            repository_root,
            working_directory,
            token_estimate: "ceil(UTF-8 bytes / 4)",
            canonical: canonical.report,
            sources,
            chains,
            placement_findings,
            findings: inventory.findings,
        })
    }

    pub fn print_human(&self) {
        println!("Instruction doctor (read-only)");
        println!("Repository: {}", self.repository_root.display());
        println!("Working directory: {}", self.working_directory.display());
        match &self.canonical.path {
            Some(path) => println!("Canonical source: {path} ({})", self.canonical.basis),
            None => println!("Canonical source: unresolved ({})", self.canonical.basis),
        }

        println!("\nSources");
        if self.sources.is_empty() {
            println!("  none discovered");
        }
        for source in &self.sources {
            println!(
                "  {} — {} lines, {} bytes, ~{} tokens; markers {}; routing {}",
                source.path,
                source.line_count,
                source.byte_count,
                source.estimated_tokens,
                source.marker_health.status,
                source.routing_asset_drift.status
            );
            if let Some(symlink) = &source.symlink {
                println!(
                    "    symlink -> {} ({})",
                    symlink.target,
                    if symlink.safe { "safe" } else { "unsafe" }
                );
            }
        }

        for chain in &self.chains {
            println!("\n{} [{}]", chain.harness, chain.support);
            match chain.project_document_max_bytes {
                Some(limit) => println!(
                    "  loaded {} bytes; project limit {limit} bytes",
                    chain.total_loaded_bytes
                ),
                None => println!("  loaded {} bytes", chain.total_loaded_bytes),
            }
            if chain.entries.is_empty() {
                println!("  no discovered sources");
            }
            for entry in &chain.entries {
                let order = entry
                    .order
                    .map_or_else(|| "-".to_string(), |value| value.to_string());
                println!(
                    "  {order}. {} [{}; {}; {}; loaded {} bytes{}] — {}",
                    entry.source,
                    entry.classification,
                    entry.load_mode,
                    if entry.effective {
                        "effective"
                    } else {
                        "not currently effective"
                    },
                    entry.loaded_bytes,
                    if entry.truncated { "; truncated" } else { "" },
                    entry.reason
                );
                if !entry.path_patterns.is_empty() {
                    println!("    paths: {}", entry.path_patterns.join(", "));
                }
            }
        }

        println!("\nPlacement diagnostics");
        if self.placement_findings.is_empty() {
            println!("  none");
        }
        for finding in &self.placement_findings {
            let location = finding.line_start.map_or_else(
                || finding.source.clone(),
                |start| match finding.line_end {
                    Some(end) if end != start => format!("{}:{start}-{end}", finding.source),
                    _ => format!("{}:{start}", finding.source),
                },
            );
            println!(
                "  {} {} [{}] — {}",
                finding.severity, finding.code, finding.category, location
            );
            println!(
                "    action {}; destination {}; protected {}",
                finding.action,
                finding.destination,
                if finding.protected { "yes" } else { "no" }
            );
            println!("    Evidence: {}", finding.evidence);
            println!("    Reason: {}", finding.rationale);
        }

        println!("\nFindings");
        if self.findings.is_empty() {
            println!("  none");
        }
        for finding in &self.findings {
            println!(
                "  {} {}: {}{}",
                finding.severity,
                finding.code,
                finding.message,
                if finding.sources.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", finding.sources.join(", "))
                }
            );
        }
    }
}

impl Inventory {
    fn new(repository_root: PathBuf) -> Self {
        Self {
            repository_root,
            sources: BTreeMap::new(),
            findings: Vec::new(),
        }
    }

    fn ensure_source(&mut self, path: &Path, scope: &str) -> Option<String> {
        let key = display_path(path, &self.repository_root);
        if self.sources.contains_key(&key) {
            return Some(key);
        }

        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
            Err(error) => {
                self.push_finding(
                    "error",
                    "instruction_source_metadata_failed",
                    format!("could not inspect {key}: {error}"),
                    vec![key.clone()],
                );
                return None;
            }
        };

        let mut symlink = None;
        let mut safe_to_read = true;
        if metadata.file_type().is_symlink() {
            let raw_target = fs::read_link(path).ok();
            let resolved_target = path.canonicalize().ok();
            let project_scoped = path.starts_with(&self.repository_root);
            let safe = resolved_target
                .as_ref()
                .is_some_and(|target| !project_scoped || target.starts_with(&self.repository_root));
            let target = resolved_target
                .as_deref()
                .map(|target| display_path(target, &self.repository_root))
                .or_else(|| raw_target.as_deref().map(path_string))
                .unwrap_or_else(|| "<unresolved>".to_string());
            symlink = Some(SymlinkReport {
                target: target.clone(),
                safe,
            });
            if !safe {
                safe_to_read = false;
                self.push_finding(
                    "error",
                    "unsafe_instruction_symlink",
                    format!("{key} resolves outside the repository or cannot be resolved"),
                    vec![key.clone(), target],
                );
            }
        }

        let (content, read_error) = if safe_to_read {
            match fs::read(path) {
                Ok(bytes) => match String::from_utf8(bytes) {
                    Ok(content) => (Some(content), None),
                    Err(error) => {
                        let message = format!("source is not UTF-8: {error}");
                        self.push_finding(
                            "error",
                            "unsupported_instruction_encoding",
                            format!("{key} is not valid UTF-8"),
                            vec![key.clone()],
                        );
                        (None, Some(message))
                    }
                },
                Err(error) => {
                    let message = error.to_string();
                    self.push_finding(
                        "error",
                        "instruction_source_read_failed",
                        format!("could not read {key}: {error}"),
                        vec![key.clone()],
                    );
                    (None, Some(message))
                }
            }
        } else {
            (None, Some("unsafe symlink target was not read".to_string()))
        };

        let (marker_health, routing_asset_drift) = content.as_deref().map_or_else(
            || {
                (
                    MarkerHealth {
                        status: "unreadable".to_string(),
                        start_count: 0,
                        end_count: 0,
                        issues: Vec::new(),
                    },
                    RoutingAssetDrift {
                        status: "unreadable".to_string(),
                        drifted: None,
                    },
                )
            },
            analyze_markers,
        );

        for issue in &marker_health.issues {
            self.push_finding(
                "error",
                &format!("routing_marker_{issue}"),
                format!("{key} has malformed Engram routing markers: {issue}"),
                vec![key.clone()],
            );
        }
        if routing_asset_drift.drifted == Some(true) {
            self.push_finding(
                "warning",
                "routing_asset_drift",
                format!("{key} has an Engram routing block that differs from this binary"),
                vec![key.clone()],
            );
        }

        let byte_count = content.as_ref().map_or(0, String::len);
        let line_count = content.as_deref().map_or(0, |value| value.lines().count());
        if scope == "project"
            && matches!(key.as_str(), "CLAUDE.md" | ".claude/CLAUDE.md")
            && line_count > 200
        {
            self.push_finding(
                "warning",
                "claude_root_over_200_lines",
                format!(
                    "{key} is {line_count} lines; Claude Code recommends keeping each CLAUDE.md under 200 lines and using scoped rules or Skills where appropriate"
                ),
                vec![key.clone()],
            );
        }
        let report = SourceReport {
            path: key.clone(),
            scope: scope.to_string(),
            line_count,
            byte_count,
            estimated_tokens: byte_count.div_ceil(4),
            marker_health,
            routing_asset_drift,
            symlink,
            read_error,
        };
        self.sources.insert(
            key.clone(),
            SourceData {
                report,
                absolute_path: path.to_path_buf(),
                content,
            },
        );
        Some(key)
    }

    fn push_finding(&mut self, severity: &str, code: &str, message: String, sources: Vec<String>) {
        self.findings.push(Finding {
            severity: severity.to_string(),
            code: code.to_string(),
            message,
            sources,
        });
    }

    fn source_content(&self, key: &str) -> Option<&str> {
        self.sources.get(key)?.content.as_deref()
    }

    fn source_bytes(&self, key: &str) -> usize {
        self.sources
            .get(key)
            .map_or(0, |source| source.report.byte_count)
    }
}

fn detect_canonical(inventory: &mut Inventory) -> CanonicalDecision {
    if let Some(configured) = explicit_canonical(inventory) {
        let explicit = path_string(Path::new(&configured));
        let path = inventory.repository_root.join(&explicit);
        let explicit_key = is_safe_relative_path(Path::new(&explicit))
            .then(|| inventory.ensure_source(&path, "project"))
            .flatten();
        let explicit_readable = explicit_key.as_ref().is_some_and(|key| {
            inventory.sources.get(key).is_some_and(|source| {
                source.content.is_some()
                    && source
                        .report
                        .symlink
                        .as_ref()
                        .is_none_or(|symlink| symlink.safe)
            })
        });
        if explicit_readable {
            let mut adapters = BTreeSet::new();
            if explicit == "AGENTS.md" {
                for source in CLAUDE_PROJECT_SOURCES {
                    if source_relates_to(inventory, source, "AGENTS.md") {
                        adapters.insert(source.to_string());
                    }
                }
            } else if CLAUDE_PROJECT_SOURCES.contains(&explicit.as_str())
                && source_relates_to(inventory, "AGENTS.md", &explicit)
            {
                adapters.insert("AGENTS.md".to_string());
            }
            return CanonicalDecision {
                report: CanonicalSource {
                    path: Some(explicit.clone()),
                    basis: "explicit_config".to_string(),
                    candidates: vec![explicit],
                },
                adapters,
            };
        }
        inventory.push_finding(
            "error",
            "invalid_explicit_canonical_source",
            format!(
                "[instructions].canonical does not name a readable in-repository file: {explicit}"
            ),
            vec![".engram.toml".to_string(), explicit.clone()],
        );
        return CanonicalDecision {
            report: CanonicalSource {
                path: None,
                basis: "invalid_explicit_config".to_string(),
                candidates: vec![explicit],
            },
            adapters: BTreeSet::new(),
        };
    }

    let claude_sources: Vec<_> = CLAUDE_PROJECT_SOURCES
        .into_iter()
        .filter(|source| inventory.source_content(source).is_some())
        .collect();
    let agents_present = inventory.source_content("AGENTS.md").is_some();
    if agents_present && !claude_sources.is_empty() {
        for source in &claude_sources {
            if source_has_safe_symlink_to(inventory, source, "AGENTS.md") {
                return canonical_with_adapter("AGENTS.md", source, "safe_symlink");
            }
        }
        for source in &claude_sources {
            if source_imports_target(inventory, source, "AGENTS.md") {
                return canonical_with_adapter("AGENTS.md", source, "claude_import");
            }
        }
        for source in &claude_sources {
            if inventory
                .source_content(source)
                .is_some_and(|content| looks_like_thin_pointer(content, "AGENTS.md"))
            {
                return canonical_with_adapter("AGENTS.md", source, "thin_pointer");
            }
        }
        for source in &claude_sources {
            if source_relates_to(inventory, "AGENTS.md", source) {
                let basis = if source_has_safe_symlink_to(inventory, "AGENTS.md", source) {
                    "safe_symlink"
                } else {
                    "thin_pointer"
                };
                return canonical_with_adapter(source, "AGENTS.md", basis);
            }
        }

        let similarity = (claude_sources.len() == 1)
            .then(|| {
                normalized_similarity(
                    inventory
                        .source_content(claude_sources[0])
                        .unwrap_or_default(),
                    inventory.source_content("AGENTS.md").unwrap_or_default(),
                )
            })
            .flatten();
        let near_duplicate = similarity.is_some_and(|score| score >= NEAR_DUPLICATE_THRESHOLD);
        let (basis, code, message) = if near_duplicate {
            let source = claude_sources[0];
            (
                "ambiguous_near_duplicate_sources",
                "near_duplicate_instruction_sources",
                format!(
                    "{source} and AGENTS.md are near duplicates (similarity {:.3}); choose a canonical source explicitly",
                    similarity.unwrap_or_default()
                ),
            )
        } else {
            (
                "ambiguous_independent_sources",
                "independent_instruction_sources",
                "Claude Code and Codex project files contain independent rules; no canonical source was inferred"
                    .to_string(),
            )
        };
        let mut candidates: Vec<_> = claude_sources.iter().map(ToString::to_string).collect();
        candidates.push("AGENTS.md".to_string());
        inventory.push_finding("warning", code, message, candidates.clone());
        return CanonicalDecision {
            report: CanonicalSource {
                path: None,
                basis: basis.to_string(),
                candidates,
            },
            adapters: BTreeSet::new(),
        };
    }

    let candidates: Vec<_> = ["CLAUDE.md", ".claude/CLAUDE.md", "AGENTS.md"]
        .into_iter()
        .filter(|candidate| {
            inventory
                .sources
                .get(*candidate)
                .is_some_and(|source| source.content.is_some())
        })
        .map(str::to_string)
        .collect();
    if candidates.len() == 1 {
        return CanonicalDecision {
            report: CanonicalSource {
                path: candidates.first().cloned(),
                basis: "single_source".to_string(),
                candidates,
            },
            adapters: BTreeSet::new(),
        };
    }
    CanonicalDecision {
        report: CanonicalSource {
            path: None,
            basis: if candidates.is_empty() {
                "none".to_string()
            } else {
                "ambiguous_multiple_claude_sources".to_string()
            },
            candidates,
        },
        adapters: BTreeSet::new(),
    }
}

fn source_relates_to(inventory: &Inventory, source: &str, target: &str) -> bool {
    source_has_safe_symlink_to(inventory, source, target)
        || source_imports_target(inventory, source, target)
        || inventory
            .source_content(source)
            .is_some_and(|content| looks_like_thin_pointer(content, target))
}

fn source_has_safe_symlink_to(inventory: &Inventory, source: &str, target: &str) -> bool {
    inventory
        .sources
        .get(source)
        .and_then(|source| source.report.symlink.as_ref())
        .is_some_and(|symlink| symlink.safe && symlink.target == target)
}

fn source_imports_target(inventory: &Inventory, source: &str, target: &str) -> bool {
    let Some(source) = inventory.sources.get(source) else {
        return false;
    };
    let Some(target) = inventory.sources.get(target) else {
        return false;
    };
    let Some(base) = source.absolute_path.parent() else {
        return false;
    };
    source
        .content
        .as_deref()
        .map(markdown_import_paths)
        .unwrap_or_default()
        .iter()
        .filter_map(|import| resolve_project_import(base, import, &inventory.repository_root))
        .any(|import| import == target.absolute_path)
}

fn canonical_with_adapter(canonical: &str, adapter: &str, basis: &str) -> CanonicalDecision {
    CanonicalDecision {
        report: CanonicalSource {
            path: Some(canonical.to_string()),
            basis: basis.to_string(),
            candidates: vec![canonical.to_string()],
        },
        adapters: BTreeSet::from([adapter.to_string()]),
    }
}

fn explicit_canonical(inventory: &mut Inventory) -> Option<String> {
    let path = inventory.repository_root.join(".engram.toml");
    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            inventory.push_finding(
                "error",
                "instruction_config_read_failed",
                format!("could not read .engram.toml: {error}"),
                vec![".engram.toml".to_string()],
            );
            return None;
        }
    };
    let document = match content.parse::<DocumentMut>() {
        Ok(document) => document,
        Err(error) => {
            inventory.push_finding(
                "error",
                "instruction_config_invalid",
                format!("could not parse .engram.toml: {error}"),
                vec![".engram.toml".to_string()],
            );
            return None;
        }
    };
    document
        .get("instructions")
        .and_then(|item| item.get("canonical"))
        .and_then(toml_edit::Item::as_str)
        .map(str::to_string)
}

fn build_claude_chain(
    inventory: &mut Inventory,
    working_directory: &Path,
    canonical: &CanonicalDecision,
) -> ChainReport {
    let mut entries = Vec::new();
    let mut next_order = 1;
    let mut loaded = BTreeSet::new();

    let mut ancestors: Vec<_> = working_directory
        .ancestors()
        .map(Path::to_path_buf)
        .collect();
    ancestors.reverse();
    for directory in ancestors {
        if directory.parent().is_none() {
            continue;
        }
        let scope = if directory.starts_with(&inventory.repository_root) {
            "project"
        } else {
            "ancestor"
        };
        let mut relative_files = vec!["CLAUDE.md", "CLAUDE.local.md"];
        if directory == inventory.repository_root {
            relative_files.insert(1, ".claude/CLAUDE.md");
        }
        for relative in relative_files {
            let path = directory.join(relative);
            let Some(key) = inventory.ensure_source(&path, scope) else {
                continue;
            };
            if loaded.contains(&key) {
                continue;
            }
            let nested = path
                .strip_prefix(&inventory.repository_root)
                .ok()
                .and_then(Path::parent)
                .is_some_and(|parent| parent != Path::new("") && parent != Path::new(".claude"));
            let classification = if nested { Some("path_scoped") } else { None };
            append_claude_source_with_classification(
                inventory,
                &mut entries,
                &mut next_order,
                &mut loaded,
                &key,
                canonical,
                "startup",
                "Claude Code concatenates ancestor instructions from filesystem root to the working directory",
                Vec::new(),
                0,
                &mut BTreeSet::new(),
                classification,
            );
        }
    }

    if canonical.report.basis == "thin_pointer"
        && canonical.adapters.contains("CLAUDE.md")
        && let Some(target) = canonical.report.path.as_ref()
        && inventory.sources.contains_key(target)
        && !entries.iter().any(|entry| entry.source == *target)
    {
        entries.push(ChainEntry {
            order: None,
            source: target.clone(),
            classification: "canonical".to_string(),
            load_mode: "referenced".to_string(),
            effective: false,
            reason: "CLAUDE.md is a thin pointer, not Claude Code @path import syntax; the target is not guaranteed to load at startup"
                .to_string(),
            loaded_bytes: 0,
            truncated: false,
            path_patterns: Vec::new(),
        });
    }

    let root_rules = inventory.repository_root.join(".claude/rules");
    for path in walk_files(&root_rules) {
        if path.extension().and_then(|value| value.to_str()) != Some("md") {
            continue;
        }
        let Some(key) = inventory.ensure_source(&path, "project") else {
            continue;
        };
        let patterns = inventory
            .source_content(&key)
            .map(parse_rule_paths)
            .unwrap_or_default();
        if patterns.is_empty() {
            push_chain_entry(
                inventory,
                &mut entries,
                &mut next_order,
                &key,
                classification_for(&key, canonical, Some("tool_specific")),
                "startup",
                true,
                "Claude Code loads unscoped .claude/rules Markdown at startup",
                Vec::new(),
                false,
                None,
            );
        } else {
            push_chain_entry(
                inventory,
                &mut entries,
                &mut next_order,
                &key,
                classification_for(&key, canonical, Some("path_scoped")),
                "path_scoped",
                false,
                "Claude Code loads this rule when it reads a file matching paths frontmatter",
                patterns,
                false,
                None,
            );
        }
        loaded.insert(key);
    }

    for path in walk_files(&inventory.repository_root) {
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if !matches!(name, "CLAUDE.md" | "CLAUDE.local.md") {
            continue;
        }
        let Some(key) = inventory.ensure_source(&path, "project") else {
            continue;
        };
        if loaded.contains(&key) {
            continue;
        }
        push_chain_entry(
            inventory,
            &mut entries,
            &mut next_order,
            &key,
            classification_for(&key, canonical, Some("path_scoped")),
            "on_demand",
            false,
            "Claude Code discovers descendant CLAUDE.md files when it reads files in that subtree",
            Vec::new(),
            false,
            Some(0),
        );
        loaded.insert(key);
    }

    let total_loaded_bytes = entries
        .iter()
        .filter(|entry| entry.effective)
        .map(|entry| entry.loaded_bytes)
        .sum();
    ChainReport {
        harness: "claude_code".to_string(),
        support: "formal".to_string(),
        total_loaded_bytes,
        project_document_max_bytes: None,
        entries,
    }
}

#[allow(clippy::too_many_arguments)]
fn append_claude_source(
    inventory: &mut Inventory,
    entries: &mut Vec<ChainEntry>,
    next_order: &mut usize,
    loaded: &mut BTreeSet<String>,
    key: &str,
    canonical: &CanonicalDecision,
    load_mode: &str,
    reason: &str,
    path_patterns: Vec<String>,
    import_depth: usize,
    import_stack: &mut BTreeSet<String>,
) {
    append_claude_source_with_classification(
        inventory,
        entries,
        next_order,
        loaded,
        key,
        canonical,
        load_mode,
        reason,
        path_patterns,
        import_depth,
        import_stack,
        None,
    );
}

#[allow(clippy::too_many_arguments)]
fn append_claude_source_with_classification(
    inventory: &mut Inventory,
    entries: &mut Vec<ChainEntry>,
    next_order: &mut usize,
    loaded: &mut BTreeSet<String>,
    key: &str,
    canonical: &CanonicalDecision,
    load_mode: &str,
    reason: &str,
    path_patterns: Vec<String>,
    import_depth: usize,
    import_stack: &mut BTreeSet<String>,
    forced_classification: Option<&str>,
) {
    let classification = forced_classification.map_or_else(
        || classification_for(key, canonical, Some("tool_specific")),
        |default| classification_for(key, canonical, Some(default)),
    );
    push_chain_entry(
        inventory,
        entries,
        next_order,
        key,
        classification,
        load_mode,
        true,
        reason,
        path_patterns,
        false,
        None,
    );
    loaded.insert(key.to_string());

    let symlink_target = inventory
        .sources
        .get(key)
        .and_then(|source| source.report.symlink.as_ref())
        .filter(|symlink| symlink.safe)
        .map(|symlink| symlink.target.clone());
    if let Some(target) = symlink_target
        && target != key
        && inventory.sources.contains_key(&target)
    {
        push_chain_entry(
            inventory,
            entries,
            next_order,
            &target,
            classification_for(&target, canonical, Some("tool_specific")),
            "symlink_target",
            false,
            &format!(
                "{key} resolves to this safe in-repository target; its bytes are counted through the adapter"
            ),
            Vec::new(),
            false,
            Some(0),
        );
        loaded.insert(target);
        return;
    }

    if import_depth >= CLAUDE_IMPORT_MAX_DEPTH {
        let imports_present = inventory
            .source_content(key)
            .is_some_and(|content| !markdown_import_paths(content).is_empty());
        if imports_present {
            inventory.push_finding(
                "warning",
                "claude_import_depth_exceeded",
                format!("{key} imports beyond Claude Code's five-hop limit"),
                vec![key.to_string()],
            );
        }
        return;
    }

    let Some(source) = inventory.sources.get(key) else {
        return;
    };
    let imports = source
        .content
        .as_deref()
        .map(markdown_import_paths)
        .unwrap_or_default();
    let base = source.absolute_path.parent().map(Path::to_path_buf);
    let Some(base) = base else {
        return;
    };

    import_stack.insert(key.to_string());
    for import in imports {
        let Some(path) = resolve_project_import(&base, &import, &inventory.repository_root) else {
            inventory.push_finding(
                "warning",
                "external_or_invalid_claude_import",
                format!("{key} imports {import}, which is outside the repository or invalid"),
                vec![key.to_string(), import],
            );
            continue;
        };
        let Some(import_key) = inventory.ensure_source(&path, "imported") else {
            inventory.push_finding(
                "error",
                "unresolved_claude_import",
                format!("{key} imports a missing file: {import}"),
                vec![key.to_string(), import],
            );
            continue;
        };
        if import_stack.contains(&import_key) {
            inventory.push_finding(
                "error",
                "claude_import_cycle",
                format!("Claude import cycle reaches {import_key}"),
                vec![key.to_string(), import_key],
            );
            continue;
        }
        append_claude_source(
            inventory,
            entries,
            next_order,
            loaded,
            &import_key,
            canonical,
            "imported",
            &format!("{key} imports this file with Claude Code @path syntax"),
            Vec::new(),
            import_depth + 1,
            import_stack,
        );
    }
    import_stack.remove(key);
}

fn build_codex_chain(
    inventory: &mut Inventory,
    working_directory: &Path,
    canonical: &CanonicalDecision,
) -> ChainReport {
    let settings = read_codex_settings(inventory);
    let mut entries = Vec::new();
    let mut next_order = 1;
    let mut project_loaded_bytes = 0;
    let mut encountered = BTreeSet::new();

    let directories = path_from_root_to_cwd(&inventory.repository_root, working_directory);
    for directory in directories {
        let mut candidates = vec![
            directory.join("AGENTS.override.md"),
            directory.join("AGENTS.md"),
        ];
        candidates.extend(
            settings
                .fallback_filenames
                .iter()
                .map(|filename| directory.join(filename)),
        );
        add_codex_directory_candidates(
            inventory,
            &mut entries,
            &mut next_order,
            &mut project_loaded_bytes,
            &mut encountered,
            &candidates,
            canonical,
            settings.project_doc_max_bytes,
        );
    }

    if canonical.adapters.contains("AGENTS.md")
        && let Some(target) = canonical.report.path.as_ref()
        && inventory.sources.contains_key(target)
        && !entries.iter().any(|entry| entry.source == *target)
    {
        let symlink = canonical.report.basis == "safe_symlink";
        entries.push(ChainEntry {
            order: None,
            source: target.clone(),
            classification: "canonical".to_string(),
            load_mode: if symlink {
                "symlink_target"
            } else {
                "referenced"
            }
            .to_string(),
            effective: false,
            reason: if symlink {
                "AGENTS.md resolves to this safe in-repository target; its bytes are counted through the adapter"
            } else {
                "AGENTS.md is a thin pointer; Codex does not have Claude Code @path import semantics"
            }
            .to_string(),
            loaded_bytes: 0,
            truncated: false,
            path_patterns: Vec::new(),
        });
    }

    for path in walk_files(&inventory.repository_root) {
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if name != "AGENTS.md"
            && name != "AGENTS.override.md"
            && !settings
                .fallback_filenames
                .iter()
                .any(|fallback| fallback == name)
        {
            continue;
        }
        let Some(key) = inventory.ensure_source(&path, "project") else {
            continue;
        };
        if encountered.contains(&key) {
            continue;
        }
        entries.push(ChainEntry {
            order: None,
            source: key.clone(),
            classification: "path_scoped".to_string(),
            load_mode: "cwd_scoped".to_string(),
            effective: false,
            reason: "Codex loads this file only when the working directory is inside its subtree"
                .to_string(),
            loaded_bytes: 0,
            truncated: false,
            path_patterns: Vec::new(),
        });
        encountered.insert(key);
    }

    if project_loaded_bytes >= settings.project_doc_max_bytes {
        inventory.push_finding(
            "error",
            "codex_project_document_limit_reached",
            format!(
                "Codex project instructions reached the configured {} byte limit; later guidance is truncated or skipped",
                settings.project_doc_max_bytes
            ),
            entries
                .iter()
                .filter(|entry| entry.truncated)
                .map(|entry| entry.source.clone())
                .collect(),
        );
    } else if project_loaded_bytes * 10 >= settings.project_doc_max_bytes * 9 {
        inventory.push_finding(
            "warning",
            "codex_project_document_limit_near",
            format!(
                "Codex project instructions use {project_loaded_bytes} of {} configured bytes",
                settings.project_doc_max_bytes
            ),
            Vec::new(),
        );
    }

    ChainReport {
        harness: "codex".to_string(),
        support: "formal".to_string(),
        total_loaded_bytes: entries
            .iter()
            .filter(|entry| entry.effective)
            .map(|entry| entry.loaded_bytes)
            .sum(),
        project_document_max_bytes: Some(settings.project_doc_max_bytes),
        entries,
    }
}

#[allow(clippy::too_many_arguments)]
fn add_codex_directory_candidates(
    inventory: &mut Inventory,
    entries: &mut Vec<ChainEntry>,
    next_order: &mut usize,
    project_loaded_bytes: &mut usize,
    encountered: &mut BTreeSet<String>,
    candidates: &[PathBuf],
    canonical: &CanonicalDecision,
    project_limit: usize,
) {
    let mut present = Vec::new();
    let mut selected = None;
    for path in candidates {
        let Some(key) = inventory.ensure_source(path, "project") else {
            continue;
        };
        let non_empty = inventory
            .source_content(&key)
            .is_some_and(|content| !content.trim().is_empty());
        present.push(key.clone());
        if selected.is_none() && non_empty {
            selected = Some(key);
        }
    }

    let selected_name = selected.clone();
    for key in present {
        encountered.insert(key.clone());
        if selected_name.as_deref() != Some(key.as_str()) {
            let default_classification = if key.ends_with("AGENTS.override.md") {
                "override"
            } else {
                "tool_specific"
            };
            entries.push(ChainEntry {
                order: None,
                source: key.clone(),
                classification: classification_for(
                    &key,
                    canonical,
                    Some(default_classification),
                ),
                load_mode: "shadowed".to_string(),
                effective: false,
                reason: selected_name.as_ref().map_or_else(
                    || "Codex skips empty instruction files".to_string(),
                    |selected| {
                        format!(
                            "Codex selects {selected} first at this directory level and ignores this file"
                        )
                    },
                ),
                loaded_bytes: 0,
                truncated: false,
                path_patterns: Vec::new(),
            });
        }
    }

    let Some(selected) = selected_name else {
        return;
    };
    let byte_count = inventory.source_bytes(&selected);
    let remaining = project_limit.saturating_sub(*project_loaded_bytes);
    let (loaded_bytes, truncated, effective, reason) = if remaining == 0 {
        (
            0,
            false,
            false,
            "Codex already reached project_doc_max_bytes before this source".to_string(),
        )
    } else {
        let loaded_bytes = byte_count.min(remaining);
        *project_loaded_bytes += loaded_bytes;
        (
            loaded_bytes,
            loaded_bytes < byte_count,
            true,
            "Codex selects the first non-empty instruction file at this directory level"
                .to_string(),
        )
    };
    let nested = selected.contains('/');
    let default_classification = if selected.ends_with("AGENTS.override.md") {
        "override"
    } else if nested {
        "path_scoped"
    } else {
        "tool_specific"
    };
    entries.push(ChainEntry {
        order: effective.then(|| {
            let order = *next_order;
            *next_order += 1;
            order
        }),
        source: selected.clone(),
        classification: classification_for(&selected, canonical, Some(default_classification)),
        load_mode: "startup".to_string(),
        effective,
        reason,
        loaded_bytes,
        truncated,
        path_patterns: Vec::new(),
    });
}

fn read_codex_settings(inventory: &mut Inventory) -> CodexSettings {
    let home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join(".codex")));
    let mut settings = CodexSettings {
        home,
        fallback_filenames: Vec::new(),
        project_doc_max_bytes: DEFAULT_CODEX_PROJECT_DOC_MAX_BYTES,
    };
    let Some(config_path) = settings.home.as_ref().map(|home| home.join("config.toml")) else {
        return settings;
    };
    let content = match fs::read_to_string(&config_path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return settings,
        Err(error) => {
            inventory.push_finding(
                "warning",
                "codex_config_read_failed",
                format!("could not read {}: {error}", config_path.display()),
                vec![config_path.display().to_string()],
            );
            return settings;
        }
    };
    let document = match content.parse::<DocumentMut>() {
        Ok(document) => document,
        Err(error) => {
            inventory.push_finding(
                "warning",
                "codex_config_invalid",
                format!("could not parse {}: {error}", config_path.display()),
                vec![config_path.display().to_string()],
            );
            return settings;
        }
    };
    if let Some(array) = document
        .get("project_doc_fallback_filenames")
        .and_then(toml_edit::Item::as_array)
    {
        for filename in array.iter().filter_map(toml_edit::Value::as_str) {
            let path = Path::new(filename);
            if matches!(path.components().next(), Some(Component::Normal(_)))
                && path.components().count() == 1
            {
                settings.fallback_filenames.push(filename.to_string());
            } else {
                inventory.push_finding(
                    "warning",
                    "invalid_codex_fallback_filename",
                    format!(
                        "Codex fallback filename is not a single safe filename and was ignored: {filename}"
                    ),
                    vec![config_path.display().to_string()],
                );
            }
        }
    }
    if let Some(value) = document
        .get("project_doc_max_bytes")
        .and_then(toml_edit::Item::as_integer)
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| *value > 0)
    {
        settings.project_doc_max_bytes = value;
    }
    settings
}

fn build_best_effort_chains(inventory: &mut Inventory) -> Vec<ChainReport> {
    let known = [
        ("gemini_cli", "GEMINI.md"),
        ("cursor", ".cursorrules"),
        ("github_copilot", ".github/copilot-instructions.md"),
    ];
    let mut chains = Vec::new();
    for (harness, relative) in known {
        let path = inventory.repository_root.join(relative);
        let Some(key) = inventory.ensure_source(&path, "project") else {
            continue;
        };
        inventory.push_finding(
            "info",
            "best_effort_harness",
            format!("{relative} is inventoried by filename only; {harness} loading semantics are not formally modeled"),
            vec![key.clone()],
        );
        chains.push(ChainReport {
            harness: harness.to_string(),
            support: "best_effort".to_string(),
            total_loaded_bytes: 0,
            project_document_max_bytes: None,
            entries: vec![ChainEntry {
                order: None,
                source: key,
                classification: "tool_specific".to_string(),
                load_mode: "best_effort".to_string(),
                effective: false,
                reason:
                    "recognized filename only; effective loading and precedence are not inferred"
                        .to_string(),
                loaded_bytes: 0,
                truncated: false,
                path_patterns: Vec::new(),
            }],
        });
    }
    chains
}

#[allow(clippy::too_many_arguments)]
fn push_chain_entry(
    inventory: &mut Inventory,
    entries: &mut Vec<ChainEntry>,
    next_order: &mut usize,
    key: &str,
    classification: String,
    load_mode: &str,
    effective: bool,
    reason: &str,
    path_patterns: Vec<String>,
    truncated: bool,
    loaded_bytes: Option<usize>,
) {
    let bytes = loaded_bytes.unwrap_or_else(|| inventory.source_bytes(key));
    let order = effective.then(|| {
        let order = *next_order;
        *next_order += 1;
        order
    });
    entries.push(ChainEntry {
        order,
        source: key.to_string(),
        classification,
        load_mode: load_mode.to_string(),
        effective,
        reason: reason.to_string(),
        loaded_bytes: if effective { bytes } else { 0 },
        truncated,
        path_patterns,
    });
}

fn classification_for(key: &str, canonical: &CanonicalDecision, default: Option<&str>) -> String {
    if canonical.report.path.as_deref() == Some(key) {
        "canonical".to_string()
    } else if canonical.adapters.contains(key) {
        "adapter".to_string()
    } else {
        default.unwrap_or("tool_specific").to_string()
    }
}

fn analyze_markers(content: &str) -> (MarkerHealth, RoutingAssetDrift) {
    let start_positions = structural_marker_positions(content, MARKER_START);
    let end_positions = structural_marker_positions(content, MARKER_END);
    let start_count = start_positions.len();
    let end_count = end_positions.len();
    if start_count == 0 && end_count == 0 {
        return (
            MarkerHealth {
                status: "absent".to_string(),
                start_count,
                end_count,
                issues: Vec::new(),
            },
            RoutingAssetDrift {
                status: "not_managed".to_string(),
                drifted: None,
            },
        );
    }

    let mut events = Vec::new();
    events.extend(start_positions.iter().copied().map(|index| (index, true)));
    events.extend(end_positions.iter().copied().map(|index| (index, false)));
    events.sort_by_key(|(index, is_start)| (*index, !*is_start));
    let mut depth = 0usize;
    let mut pairs = 0usize;
    let mut issues = BTreeSet::new();
    for (_, is_start) in events {
        if is_start {
            if depth > 0 {
                issues.insert("nested".to_string());
            }
            depth += 1;
        } else if depth == 0 {
            issues.insert("crossed".to_string());
        } else {
            depth -= 1;
            if depth == 0 {
                pairs += 1;
            }
        }
    }
    if start_count == 0 {
        issues.insert("missing_start".to_string());
    }
    if end_count == 0 {
        issues.insert("missing_end".to_string());
    }
    if start_count != end_count || depth != 0 {
        issues.insert("incomplete".to_string());
    }
    if pairs > 1 {
        issues.insert("duplicate".to_string());
    }
    let issues: Vec<_> = issues.into_iter().collect();
    if !issues.is_empty() {
        return (
            MarkerHealth {
                status: "invalid".to_string(),
                start_count,
                end_count,
                issues,
            },
            RoutingAssetDrift {
                status: "unknown_invalid_markers".to_string(),
                drifted: None,
            },
        );
    }

    let actual = start_positions
        .first()
        .zip(end_positions.first())
        .and_then(|(start, end)| {
            let body_start = *start + MARKER_START.len();
            (*end >= body_start).then_some(&content[body_start..*end])
        })
        .unwrap_or_default();
    let drifted = normalize_newlines(actual).trim() != SNIPPET_BODY.trim();
    (
        MarkerHealth {
            status: "healthy".to_string(),
            start_count,
            end_count,
            issues,
        },
        RoutingAssetDrift {
            status: if drifted { "drifted" } else { "current" }.to_string(),
            drifted: Some(drifted),
        },
    )
}

fn structural_marker_positions(content: &str, marker: &str) -> Vec<usize> {
    let mut positions = Vec::new();
    let mut offset = 0usize;
    let mut fence: Option<&str> = None;
    for line in content.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if let Some(active) = fence {
            if trimmed.starts_with(active) {
                fence = None;
            }
            offset += line.len();
            continue;
        }
        if trimmed.starts_with("```") {
            fence = Some("```");
            offset += line.len();
            continue;
        }
        if trimmed.starts_with("~~~") {
            fence = Some("~~~");
            offset += line.len();
            continue;
        }

        let mut inline_code = false;
        let mut index = 0usize;
        while index < line.len() {
            let remainder = &line[index..];
            let Some(character) = remainder.chars().next() else {
                break;
            };
            if character == '`' {
                inline_code = !inline_code;
                index += character.len_utf8();
                continue;
            }
            if !inline_code && remainder.starts_with(marker) {
                positions.push(offset + index);
                index += marker.len();
                continue;
            }
            index += character.len_utf8();
        }
        offset += line.len();
    }
    positions
}

fn normalize_newlines(content: &str) -> String {
    content.replace("\r\n", "\n").replace('\r', "\n")
}

fn markdown_import_paths(content: &str) -> Vec<String> {
    let mut imports = Vec::new();
    let mut fence: Option<&str> = None;
    for line in content.lines() {
        let trimmed = line.trim_start();
        if let Some(marker) = fence {
            if trimmed.starts_with(marker) {
                fence = None;
            }
            continue;
        }
        if trimmed.starts_with("```") {
            fence = Some("```");
            continue;
        }
        if trimmed.starts_with("~~~") {
            fence = Some("~~~");
            continue;
        }
        let outside_inline: String = line
            .split('`')
            .enumerate()
            .filter_map(|(index, part)| (index % 2 == 0).then_some(part))
            .collect::<Vec<_>>()
            .join(" ");
        let characters: Vec<_> = outside_inline.char_indices().collect();
        for (position, (byte_index, character)) in characters.iter().enumerate() {
            if *character != '@' {
                continue;
            }
            if position > 0
                && (characters[position - 1].1.is_alphanumeric()
                    || matches!(characters[position - 1].1, '_' | '.'))
            {
                continue;
            }
            let start = *byte_index + character.len_utf8();
            let mut end = outside_inline.len();
            for (_, (candidate_index, candidate)) in
                characters.iter().enumerate().skip(position + 1)
            {
                if !candidate.is_alphanumeric() && !matches!(candidate, '/' | '.' | '_' | '-' | '~')
                {
                    end = *candidate_index;
                    break;
                }
            }
            let value = outside_inline[start..end].trim().trim_end_matches('.');
            if !value.is_empty() {
                imports.push(value.to_string());
            }
        }
    }
    imports
}

fn resolve_project_import(base: &Path, import: &str, repository_root: &Path) -> Option<PathBuf> {
    let import_path = Path::new(import);
    if import.starts_with('~') || import_path.is_absolute() {
        return None;
    }
    let joined = lexical_normalize(&base.join(import_path))?;
    joined.starts_with(repository_root).then_some(joined)
}

fn lexical_normalize(path: &Path) -> Option<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            Component::Normal(value) => normalized.push(value),
        }
    }
    Some(normalized)
}

fn looks_like_thin_pointer(content: &str, target: &str) -> bool {
    let content = content_without_routing_blocks(content);
    let meaningful: Vec<_> = content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("<!--"))
        .collect();
    meaningful.len() <= 10
        && content.len() <= 800
        && meaningful.iter().any(|line| line.contains(target))
}

fn normalized_similarity(left: &str, right: &str) -> Option<f64> {
    let trigrams = |content: &str| -> BTreeSet<String> {
        let characters: Vec<_> = content_without_routing_blocks(content)
            .chars()
            .filter(|character| character.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect();
        characters
            .windows(3)
            .map(|window| window.iter().collect())
            .collect()
    };
    let left = trigrams(left);
    let right = trigrams(right);
    if left.len().min(right.len()) < 24 {
        return None;
    }
    let common = left.intersection(&right).count();
    let total = left.len() + right.len();
    (total != 0).then_some((2 * common) as f64 / total as f64)
}

fn content_without_routing_blocks(content: &str) -> String {
    let starts = structural_marker_positions(content, MARKER_START);
    let ends = structural_marker_positions(content, MARKER_END);
    let mut output = String::new();
    let mut cursor = 0;
    let mut ends = ends.into_iter().peekable();
    for start in starts {
        while ends.peek().is_some_and(|end| *end < start) {
            ends.next();
        }
        let Some(end) = ends.next() else {
            break;
        };
        output.push_str(&content[cursor..start]);
        cursor = end + MARKER_END.len();
    }
    output.push_str(&content[cursor..]);
    output
}

fn parse_rule_paths(content: &str) -> Vec<String> {
    let mut lines = content.lines();
    if lines.next().map(str::trim) != Some("---") {
        return Vec::new();
    }
    let mut in_paths = false;
    let mut paths = Vec::new();
    for line in lines {
        let trimmed = line.trim();
        if trimmed == "---" {
            break;
        }
        if let Some(value) = trimmed.strip_prefix("paths:") {
            in_paths = true;
            let inline = value.trim().trim_start_matches('[').trim_end_matches(']');
            if !inline.is_empty() {
                paths.extend(
                    inline
                        .split(',')
                        .map(|value| trim_yaml_string(value.trim()))
                        .filter(|value| !value.is_empty()),
                );
            }
            continue;
        }
        if in_paths {
            if let Some(value) = trimmed.strip_prefix('-') {
                let value = trim_yaml_string(value.trim());
                if !value.is_empty() {
                    paths.push(value);
                }
            } else if !line.starts_with(' ') && !line.starts_with('\t') {
                in_paths = false;
            }
        }
    }
    paths
}

fn trim_yaml_string(value: &str) -> String {
    value
        .trim_matches(|character| matches!(character, '\'' | '"'))
        .to_string()
}

fn is_safe_relative_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
}

fn path_from_root_to_cwd(root: &Path, cwd: &Path) -> Vec<PathBuf> {
    if !cwd.starts_with(root) {
        return vec![root.to_path_buf()];
    }
    let mut result = vec![root.to_path_buf()];
    let Ok(relative) = cwd.strip_prefix(root) else {
        return result;
    };
    let mut current = root.to_path_buf();
    for component in relative.components() {
        if let Component::Normal(value) = component {
            current.push(value);
            result.push(current.clone());
        }
    }
    result
}

fn walk_files(root: &Path) -> Vec<PathBuf> {
    fn visit(directory: &Path, output: &mut Vec<PathBuf>) {
        let mut entries: Vec<_> = match fs::read_dir(directory) {
            Ok(entries) => entries.flatten().collect(),
            Err(_) => return,
        };
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.file_type().is_symlink() || metadata.is_file() {
                output.push(path);
            } else if metadata.is_dir() {
                let name = entry.file_name();
                if matches!(name.to_str(), Some(".git" | "target" | "node_modules")) {
                    continue;
                }
                visit(&path, output);
            }
        }
    }
    let mut output = Vec::new();
    visit(root, &mut output);
    output
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn discover_repository_root(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors()
        .find(|ancestor| ancestor.join(".git").exists())
        .map(Path::to_path_buf)
}

fn display_path(path: &Path, repository_root: &Path) -> String {
    path.strip_prefix(repository_root)
        .map(path_string)
        .unwrap_or_else(|_| path.display().to_string())
}

fn path_string(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::{analyze_markers, markdown_import_paths, parse_rule_paths};
    use engram_core::{MARKER_END, MARKER_START};

    #[test]
    fn import_parser_skips_code_spans_and_fences() {
        let content = "Contact maintainer@example.com.\n@AGENTS.md\n`@literal.md`\n```md\n@fenced.md\n```\n@docs/rules.md\n";
        assert_eq!(
            markdown_import_paths(content),
            vec!["AGENTS.md", "docs/rules.md"]
        );
    }

    #[test]
    fn marker_parser_distinguishes_duplicate_nested_crossed_and_incomplete() {
        let duplicate = format!("{MARKER_START}\na\n{MARKER_END}\n{MARKER_START}\nb\n{MARKER_END}");
        assert!(
            analyze_markers(&duplicate)
                .0
                .issues
                .contains(&"duplicate".to_string())
        );
        let nested = format!("{MARKER_START}\n{MARKER_START}\n{MARKER_END}\n{MARKER_END}");
        assert!(
            analyze_markers(&nested)
                .0
                .issues
                .contains(&"nested".to_string())
        );
        let crossed = format!("{MARKER_END}\n{MARKER_START}");
        assert!(
            analyze_markers(&crossed)
                .0
                .issues
                .contains(&"crossed".to_string())
        );
        assert!(
            analyze_markers(MARKER_START)
                .0
                .issues
                .contains(&"incomplete".to_string())
        );
    }

    #[test]
    fn rule_parser_reads_block_and_inline_paths() {
        assert_eq!(
            parse_rule_paths("---\npaths:\n  - \"src/**/*.rs\"\n---\n# Rule\n"),
            vec!["src/**/*.rs"]
        );
        assert_eq!(
            parse_rule_paths("---\npaths: [\"src/**\", 'tests/**']\n---\n"),
            vec!["src/**", "tests/**"]
        );
    }
}
