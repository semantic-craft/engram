//! Deterministic placement diagnostics for project instruction content.
//!
//! The analyzer deliberately uses only repository files and already-built
//! instruction chains. It does not construct an LLM provider or open Engram
//! state. Its recommendations are diagnostics: they never edit or stage
//! instruction content.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Component, Path, PathBuf};

use engram_core::{MARKER_END, MARKER_START};
use serde::Serialize;

#[derive(Debug, Clone)]
pub(crate) struct PlacementSource {
    pub path: String,
    pub absolute_path: PathBuf,
    pub content: String,
    pub line_count: usize,
    pub safe_symlink_target: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct PlacementChain {
    pub harness: String,
    pub total_loaded_bytes: usize,
    pub project_document_max_bytes: Option<usize>,
    pub entries: Vec<PlacementChainEntry>,
}

#[derive(Debug, Clone)]
pub(crate) struct PlacementChainEntry {
    pub source: String,
    pub load_mode: String,
    pub effective: bool,
    pub loaded_bytes: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlacementAction {
    Keep,
    Move,
    Remove,
    Reinforce,
    Review,
}

impl fmt::Display for PlacementAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Keep => "keep",
            Self::Move => "move",
            Self::Remove => "remove",
            Self::Reinforce => "reinforce",
            Self::Review => "review",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlacementDestination {
    RootInstructions,
    PathRules,
    AgentSkill,
    Wiki,
    Enforcement,
    NoChange,
}

impl fmt::Display for PlacementDestination {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::RootInstructions => "root_instructions",
            Self::PathRules => "path_rules",
            Self::AgentSkill => "agent_skill",
            Self::Wiki => "wiki",
            Self::Enforcement => "enforcement",
            Self::NoChange => "no_change",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlacementCategory {
    GenericHarness,
    TeamConvention,
    PrivateDeployment,
    InternalTool,
    DatabaseMigration,
    BusinessBoundary,
    SecurityRequirement,
    ComponentScope,
    Workflow,
    HistoryAndEvidence,
    Duplication,
    Contradiction,
    StaleReference,
    MissingSkill,
    ContextBudget,
}

impl fmt::Display for PlacementCategory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::GenericHarness => "generic_harness",
            Self::TeamConvention => "team_convention",
            Self::PrivateDeployment => "private_deployment",
            Self::InternalTool => "internal_tool",
            Self::DatabaseMigration => "database_migration",
            Self::BusinessBoundary => "business_boundary",
            Self::SecurityRequirement => "security_requirement",
            Self::ComponentScope => "component_scope",
            Self::Workflow => "workflow",
            Self::HistoryAndEvidence => "history_and_evidence",
            Self::Duplication => "duplication",
            Self::Contradiction => "contradiction",
            Self::StaleReference => "stale_reference",
            Self::MissingSkill => "missing_skill",
            Self::ContextBudget => "context_budget",
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PlacementFinding {
    pub severity: String,
    pub code: String,
    pub category: PlacementCategory,
    pub source: String,
    pub line_start: Option<usize>,
    pub line_end: Option<usize>,
    pub action: PlacementAction,
    pub destination: PlacementDestination,
    pub protected: bool,
    pub evidence: String,
    pub rationale: String,
    pub related_sources: Vec<String>,
}

#[derive(Debug, Clone)]
struct GuidanceUnit {
    source: String,
    line_start: usize,
    line_end: usize,
    heading: String,
    text: String,
    current_destination: PlacementDestination,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingKind {
    Paragraph,
    NumberedList,
}

pub(crate) fn analyze(
    repository_root: &Path,
    sources: Vec<PlacementSource>,
    chains: Vec<PlacementChain>,
) -> Vec<PlacementFinding> {
    let source_paths: BTreeSet<_> = sources.iter().map(|source| source.path.clone()).collect();
    let mut units = Vec::new();
    for source in &sources {
        if !source.absolute_path.starts_with(repository_root) {
            continue;
        }
        if source
            .safe_symlink_target
            .as_ref()
            .is_some_and(|target| target != &source.path && source_paths.contains(target))
        {
            continue;
        }
        units.extend(parse_guidance_units(source));
    }

    let mut findings = Vec::new();
    for unit in &units {
        if let Some(finding) = classify_unit(unit, repository_root) {
            findings.push(finding);
        }
        findings.extend(reference_findings(unit, repository_root));
    }
    findings.extend(duplicate_findings(&units));
    findings.extend(contradiction_findings(&units));
    findings.extend(context_budget_findings(&sources, &chains));

    findings.sort_by(|left, right| {
        (
            &left.source,
            left.line_start,
            left.line_end,
            &left.code,
            left.category,
            left.action,
            left.destination,
        )
            .cmp(&(
                &right.source,
                right.line_start,
                right.line_end,
                &right.code,
                right.category,
                right.action,
                right.destination,
            ))
    });
    findings
}

fn classify_unit(unit: &GuidanceUnit, repository_root: &Path) -> Option<PlacementFinding> {
    let combined = format!("{}\n{}", unit.heading, unit.text);
    let lower = combined.to_lowercase();
    let project_category = protected_category(&lower);

    if project_category.is_none()
        && let Some(phrase) = generic_harness_phrase(&lower)
    {
        return Some(unit_finding(
            unit,
            "info",
            "generic_harness_guidance",
            PlacementCategory::GenericHarness,
            PlacementAction::Remove,
            PlacementDestination::NoChange,
            false,
            format!("Matched generic harness phrase: {phrase}"),
            "The text only tells a capable model how to reason or behave and contains no repository-specific constraint; removal requires no relocation target."
                .to_string(),
            Vec::new(),
        ));
    }

    if is_multi_step_workflow(&unit.text) {
        let protected = project_category.is_some();
        return Some(unit_finding(
            unit,
            "info",
            "workflow_in_always_loaded_context",
            PlacementCategory::Workflow,
            PlacementAction::Move,
            PlacementDestination::AgentSkill,
            protected,
            "Three or more ordered steps form an on-demand procedure".to_string(),
            if protected {
                "The procedure contains project-specific knowledge that cannot be inferred; an Agent Skill preserves it while loading it only for the relevant task."
            } else {
                "Multi-step procedures are task-specific and fit progressive disclosure through an Agent Skill."
            }
            .to_string(),
            Vec::new(),
        ));
    }

    if project_category == Some(PlacementCategory::SecurityRequirement) && is_mandatory(&lower) {
        return Some(unit_finding(
            unit,
            "warning",
            "mandatory_control_requires_enforcement",
            PlacementCategory::SecurityRequirement,
            PlacementAction::Reinforce,
            PlacementDestination::Enforcement,
            true,
            short_evidence(&unit.text),
            "Mandatory project security requirements cannot be inferred reliably, and prose is not enforcement; preserve the requirement and enforce it with permissions, hooks, sandboxing, authentication, or an equivalent control."
                .to_string(),
            Vec::new(),
        ));
    }

    if is_component_scoped(&lower, repository_root) {
        return Some(unit_finding(
            unit,
            "info",
            "component_rule_in_broader_context",
            PlacementCategory::ComponentScope,
            PlacementAction::Move,
            PlacementDestination::PathRules,
            true,
            short_evidence(&unit.text),
            "The constraint names a repository subtree and cannot be inferred for that component; a path-scoped rule preserves it without loading it for unrelated work."
                .to_string(),
            Vec::new(),
        ));
    }

    if is_history_or_evidence(&lower) {
        return Some(unit_finding(
            unit,
            "info",
            "history_or_evidence_in_instructions",
            PlacementCategory::HistoryAndEvidence,
            PlacementAction::Move,
            PlacementDestination::Wiki,
            true,
            short_evidence(&unit.text),
            "Historical rationale, evidence, and rejected alternatives are project knowledge that cannot be inferred, but they belong in the Engram Wiki rather than every-session instructions."
                .to_string(),
            Vec::new(),
        ));
    }

    let category = project_category?;
    if category == PlacementCategory::PrivateDeployment {
        return Some(unit_finding(
            unit,
            "info",
            "private_deployment_in_always_loaded_context",
            category,
            PlacementAction::Move,
            PlacementDestination::AgentSkill,
            true,
            short_evidence(&unit.text),
            "Private deployment knowledge cannot be inferred; an Agent Skill keeps the procedure available without loading it for unrelated tasks."
                .to_string(),
            Vec::new(),
        ));
    }

    let destination = unit.current_destination;
    Some(unit_finding(
        unit,
        "info",
        "protected_project_context",
        category,
        PlacementAction::Keep,
        destination,
        true,
        short_evidence(&unit.text),
        protected_rationale(category, destination),
        Vec::new(),
    ))
}

fn protected_rationale(category: PlacementCategory, destination: PlacementDestination) -> String {
    format!(
        "{} cannot be inferred reliably from general model knowledge or repository shape; keep it in {} unless stronger scope evidence supports a reviewed move.",
        match category {
            PlacementCategory::TeamConvention => "Team-specific coding conventions",
            PlacementCategory::InternalTool => "Internal tool boundaries",
            PlacementCategory::DatabaseMigration => "Database migration constraints",
            PlacementCategory::BusinessBoundary => "Business boundaries",
            PlacementCategory::SecurityRequirement => "Project security requirements",
            PlacementCategory::PrivateDeployment => "Private deployment procedures",
            _ => "Project-specific guidance",
        },
        destination
    )
}

#[allow(clippy::too_many_arguments)]
fn unit_finding(
    unit: &GuidanceUnit,
    severity: &str,
    code: &str,
    category: PlacementCategory,
    action: PlacementAction,
    destination: PlacementDestination,
    protected: bool,
    evidence: String,
    rationale: String,
    related_sources: Vec<String>,
) -> PlacementFinding {
    PlacementFinding {
        severity: severity.to_string(),
        code: code.to_string(),
        category,
        source: unit.source.clone(),
        line_start: Some(unit.line_start),
        line_end: Some(unit.line_end),
        action,
        destination,
        protected,
        evidence,
        rationale,
        related_sources,
    }
}

fn parse_guidance_units(source: &PlacementSource) -> Vec<GuidanceUnit> {
    let current_destination = source_destination(&source.path, &source.content);
    let mut units = Vec::new();
    let mut heading = String::new();
    let mut pending = Vec::new();
    let mut pending_kind = None;
    let mut in_fence: Option<&str> = None;
    let mut in_routing_block = false;
    let mut in_frontmatter = source.content.lines().next().map(str::trim) == Some("---");

    for (index, line) in source.content.lines().enumerate() {
        let line_number = index + 1;
        let trimmed = line.trim();

        if in_frontmatter {
            if line_number > 1 && trimmed == "---" {
                in_frontmatter = false;
            }
            continue;
        }
        if in_routing_block {
            if trimmed.contains(MARKER_END) {
                in_routing_block = false;
            }
            continue;
        }
        if trimmed.contains(MARKER_START) {
            flush_pending(
                &mut units,
                &source.path,
                &heading,
                current_destination,
                &mut pending,
                &mut pending_kind,
            );
            in_routing_block = true;
            continue;
        }
        if let Some(marker) = in_fence {
            if trimmed.starts_with(marker) {
                in_fence = None;
            }
            continue;
        }
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            flush_pending(
                &mut units,
                &source.path,
                &heading,
                current_destination,
                &mut pending,
                &mut pending_kind,
            );
            in_fence = Some(if trimmed.starts_with("```") {
                "```"
            } else {
                "~~~"
            });
            continue;
        }
        if trimmed.is_empty() {
            if pending_kind != Some(PendingKind::NumberedList) {
                flush_pending(
                    &mut units,
                    &source.path,
                    &heading,
                    current_destination,
                    &mut pending,
                    &mut pending_kind,
                );
            }
            continue;
        }
        if trimmed.starts_with('#') {
            flush_pending(
                &mut units,
                &source.path,
                &heading,
                current_destination,
                &mut pending,
                &mut pending_kind,
            );
            heading = trimmed.trim_start_matches('#').trim().to_string();
            continue;
        }
        if trimmed.starts_with("<!--") || is_import_only(trimmed) {
            continue;
        }

        if is_bullet_item(trimmed) && is_procedure_heading(&heading) {
            if pending_kind.is_some_and(|current| current != PendingKind::NumberedList) {
                flush_pending(
                    &mut units,
                    &source.path,
                    &heading,
                    current_destination,
                    &mut pending,
                    &mut pending_kind,
                );
            }
            pending_kind = Some(PendingKind::NumberedList);
            pending.push((line_number, trimmed.to_string()));
            continue;
        }
        if is_bullet_item(trimmed) {
            flush_pending(
                &mut units,
                &source.path,
                &heading,
                current_destination,
                &mut pending,
                &mut pending_kind,
            );
            units.push(GuidanceUnit {
                source: source.path.clone(),
                line_start: line_number,
                line_end: line_number,
                heading: heading.clone(),
                text: trimmed.to_string(),
                current_destination,
            });
            continue;
        }

        let kind = if is_numbered_item(trimmed) {
            PendingKind::NumberedList
        } else {
            PendingKind::Paragraph
        };
        if pending_kind.is_some_and(|current| current != kind) {
            flush_pending(
                &mut units,
                &source.path,
                &heading,
                current_destination,
                &mut pending,
                &mut pending_kind,
            );
        }
        pending_kind = Some(kind);
        pending.push((line_number, trimmed.to_string()));
    }
    flush_pending(
        &mut units,
        &source.path,
        &heading,
        current_destination,
        &mut pending,
        &mut pending_kind,
    );
    units
}

fn flush_pending(
    units: &mut Vec<GuidanceUnit>,
    source: &str,
    heading: &str,
    current_destination: PlacementDestination,
    pending: &mut Vec<(usize, String)>,
    pending_kind: &mut Option<PendingKind>,
) {
    if let (Some((start, _)), Some((end, _))) = (pending.first(), pending.last()) {
        units.push(GuidanceUnit {
            source: source.to_string(),
            line_start: *start,
            line_end: *end,
            heading: heading.to_string(),
            text: pending
                .iter()
                .map(|(_, line)| line.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            current_destination,
        });
    }
    pending.clear();
    *pending_kind = None;
}

fn source_destination(path: &str, content: &str) -> PlacementDestination {
    let path = Path::new(path);
    let nested_instruction = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            matches!(
                name,
                "AGENTS.md" | "AGENTS.override.md" | "CLAUDE.md" | "CLAUDE.local.md"
            )
        })
        && path
            .parent()
            .is_some_and(|parent| parent != Path::new("") && parent != Path::new(".claude"));
    if nested_instruction
        || (path.starts_with(".claude/rules") && frontmatter_declares_paths(content))
    {
        PlacementDestination::PathRules
    } else {
        PlacementDestination::RootInstructions
    }
}

fn frontmatter_declares_paths(content: &str) -> bool {
    let mut lines = content.lines();
    if lines.next().map(str::trim) != Some("---") {
        return false;
    }
    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        if trimmed == "---" {
            return false;
        }
        if let Some(value) = trimmed.strip_prefix("paths:") {
            if !value.trim().is_empty() {
                return true;
            }
            for path_line in lines.by_ref() {
                let path_trimmed = path_line.trim();
                if path_trimmed == "---" {
                    return false;
                }
                if path_trimmed.starts_with('-') {
                    return true;
                }
                if !path_line.starts_with(' ') && !path_line.starts_with('\t') {
                    return false;
                }
            }
            return false;
        }
    }
    false
}

fn protected_category(lower: &str) -> Option<PlacementCategory> {
    if contains_any(
        lower,
        &[
            "production deployment",
            "private deploy",
            "deployment procedure",
            "deploy to production",
            "release workflow",
            "生产部署",
            "私有部署",
        ],
    ) {
        return Some(PlacementCategory::PrivateDeployment);
    }
    if contains_any(
        lower,
        &["internal ", "in-house", "private tool", "内网", "内部工具"],
    ) {
        return Some(PlacementCategory::InternalTool);
    }
    if contains_any(
        lower,
        &[
            "database migration",
            "schema migration",
            "migrations must",
            "migration must",
            "数据库迁移",
            "模式迁移",
        ],
    ) {
        return Some(PlacementCategory::DatabaseMigration);
    }
    if contains_any(
        lower,
        &[
            "tenant",
            "entitlement",
            "billing",
            "customer plan",
            "enterprise",
            "business boundary",
            "业务边界",
            "计费",
            "租户",
        ],
    ) {
        return Some(PlacementCategory::BusinessBoundary);
    }
    if contains_any(
        lower,
        &[
            "authentication",
            "authorization",
            "permission",
            "credential",
            "secret",
            "sandbox",
            "security requirement",
            "access control",
            "认证",
            "权限",
            "凭据",
            "密钥",
            "安全要求",
        ],
    ) {
        return Some(PlacementCategory::SecurityRequirement);
    }
    if contains_any(
        lower,
        &[
            "coding convention",
            "code convention",
            "naming convention",
            "error handling",
            "formatter",
            "rustfmt",
            "clippy",
            "cargo test",
            "pytest",
            "npm test",
            "pnpm test",
            "代码约定",
            "命名约定",
        ],
    ) || (lower.contains("this repository") && is_mandatory(lower))
    {
        return Some(PlacementCategory::TeamConvention);
    }
    None
}

fn generic_harness_phrase(lower: &str) -> Option<&'static str> {
    [
        "think step by step",
        "be helpful",
        "analyze carefully",
        "reason carefully",
        "do your best",
        "write clean code",
        "follow best practices",
        "be concise and clear",
        "you are an expert",
        "仔细思考",
        "逐步思考",
    ]
    .into_iter()
    .find(|phrase| lower.contains(phrase))
}

fn is_multi_step_workflow(text: &str) -> bool {
    text.lines()
        .filter(|line| {
            let line = line.trim();
            is_numbered_item(line) || is_bullet_item(line)
        })
        .count()
        >= 3
}

fn is_procedure_heading(heading: &str) -> bool {
    let heading = heading.to_lowercase();
    contains_any(
        &heading,
        &[
            "workflow",
            "procedure",
            "checklist",
            "release",
            "deploy",
            "verification",
            "流程",
            "步骤",
            "清单",
            "发布",
            "部署",
            "验证",
        ],
    )
}

fn is_component_scoped(lower: &str, repository_root: &Path) -> bool {
    if !contains_any(
        lower,
        &[
            "when working under",
            "when working in",
            "files under",
            "inside the component",
            "for the component",
            "在该目录",
            "目录下",
        ],
    ) {
        return false;
    }
    inline_code_spans(lower).iter().any(|value| {
        let token = value.split_whitespace().next().unwrap_or_default();
        resolve_repository_path(repository_root, token).is_some()
    })
}

fn is_history_or_evidence(lower: &str) -> bool {
    contains_any(
        lower,
        &[
            "background\n",
            "rationale\n",
            "history\n",
            "decision record\n",
            "rejected alternative",
            "benchmark evidence",
            "historical evidence",
            "背景\n",
            "历史\n",
            "决策理由",
        ],
    )
}

fn is_mandatory(lower: &str) -> bool {
    contains_any(
        lower,
        &[
            "must ",
            "must never",
            "must not",
            "never ",
            "required",
            "only supported",
            "shall ",
            "不得",
            "必须",
            "禁止",
        ],
    )
}

fn reference_findings(unit: &GuidanceUnit, repository_root: &Path) -> Vec<PlacementFinding> {
    let mut findings = Vec::new();
    let lower = unit.text.to_lowercase();
    let command_context = contains_any(
        &lower,
        &["run `", "execute `", "invoke `", "运行`", "执行`"],
    );

    for code in inline_code_spans(&unit.text) {
        let skill_reference = lower.contains("skill") && is_skill_reference(&code);
        if skill_reference && !skill_exists(repository_root, &code) {
            findings.push(unit_finding(
                unit,
                "warning",
                "missing_referenced_skill",
                PlacementCategory::MissingSkill,
                PlacementAction::Review,
                PlacementDestination::AgentSkill,
                true,
                short_evidence(&unit.text),
                "The instruction names an Agent Skill that is not present in the repository skill roots; restore the Skill or update the reference rather than dropping the workflow knowledge."
                    .to_string(),
                Vec::new(),
            ));
            continue;
        }

        let command = code.split_whitespace().next().unwrap_or_default();
        if command_context && looks_like_repository_path(command) {
            if resolve_repository_path(repository_root, command).is_none() {
                findings.push(unit_finding(
                    unit,
                "warning",
                "stale_command_reference",
                PlacementCategory::StaleReference,
                PlacementAction::Review,
                    PlacementDestination::NoChange,
                    false,
                    short_evidence(&unit.text),
                    "The referenced in-repository command path does not exist; this stale reference is removal or correction evidence independent of file length."
                        .to_string(),
                    Vec::new(),
                ));
            }
            continue;
        }

        if looks_like_repository_path(&code)
            && resolve_repository_path(repository_root, &code).is_none()
        {
            findings.push(unit_finding(
                unit,
                "warning",
                "stale_path_reference",
                PlacementCategory::StaleReference,
                PlacementAction::Review,
                PlacementDestination::NoChange,
                false,
                short_evidence(&unit.text),
                "The referenced repository path does not exist; this stale reference is removal or correction evidence independent of file length."
                    .to_string(),
                Vec::new(),
            ));
        }
    }

    for target in markdown_link_targets(&unit.text) {
        let repository_path = target.split(['#', '?']).next().unwrap_or_default();
        if looks_like_repository_path(repository_path)
            && resolve_repository_path(repository_root, repository_path).is_none()
        {
            findings.push(unit_finding(
                unit,
                "warning",
                "stale_path_reference",
                PlacementCategory::StaleReference,
                PlacementAction::Review,
                PlacementDestination::NoChange,
                false,
                short_evidence(&unit.text),
                "The relative Markdown target does not exist in the repository; staleness, not document length, supports correction or removal."
                    .to_string(),
                Vec::new(),
            ));
        }
    }
    findings
}

fn duplicate_findings(units: &[GuidanceUnit]) -> Vec<PlacementFinding> {
    let mut first_seen: BTreeMap<String, &GuidanceUnit> = BTreeMap::new();
    let mut findings = Vec::new();
    for unit in units {
        let normalized = normalize_guidance(&unit.text);
        if normalized.chars().count() < 24 {
            continue;
        }
        if let Some(first) = first_seen.get(&normalized) {
            let duplicate_kind =
                if whitespace_normalized(&unit.text) == whitespace_normalized(&first.text) {
                    "Exact"
                } else {
                    "Normalized"
                };
            findings.push(unit_finding(
                unit,
                "warning",
                "duplicate_guidance",
                PlacementCategory::Duplication,
                PlacementAction::Remove,
                PlacementDestination::NoChange,
                protected_category(&format!("{}\n{}", unit.heading, unit.text).to_lowercase())
                    .is_some(),
                format!("{duplicate_kind} duplicate of {}", unit_location(first)),
                "The same guidance already appears at the related location; normalized duplication is explicit removal evidence and needs no relocation target."
                    .to_string(),
                vec![unit_location(first)],
            ));
        } else {
            first_seen.insert(normalized, unit);
        }
    }
    findings
}

fn contradiction_findings(units: &[GuidanceUnit]) -> Vec<PlacementFinding> {
    let mut first_seen: BTreeMap<String, (bool, &GuidanceUnit)> = BTreeMap::new();
    let mut findings = Vec::new();
    for unit in units {
        let Some((positive, signature)) = directive_signature(&unit.text) else {
            continue;
        };
        if signature.chars().count() < 8 {
            continue;
        }
        if let Some((first_positive, first)) = first_seen.get(&signature) {
            if *first_positive != positive {
                findings.push(unit_finding(
                    unit,
                    "error",
                    "contradictory_guidance",
                    PlacementCategory::Contradiction,
                    PlacementAction::Review,
                    PlacementDestination::NoChange,
                    true,
                    short_evidence(&unit.text),
                    "The same normalized directive appears with opposite polarity; a maintainer must choose the intended rule, and the doctor will not resolve the conflict automatically."
                        .to_string(),
                    vec![unit_location(first)],
                ));
            }
        } else {
            first_seen.insert(signature, (positive, unit));
        }
    }
    findings
}

fn context_budget_findings(
    sources: &[PlacementSource],
    chains: &[PlacementChain],
) -> Vec<PlacementFinding> {
    let mut findings = Vec::new();
    for source in sources {
        if matches!(source.path.as_str(), "CLAUDE.md" | ".claude/CLAUDE.md")
            && source.line_count > 200
        {
            findings.push(PlacementFinding {
                severity: "warning".to_string(),
                code: "context_budget_pressure".to_string(),
                category: PlacementCategory::ContextBudget,
                source: source.path.clone(),
                line_start: None,
                line_end: None,
                action: PlacementAction::Review,
                destination: PlacementDestination::NoChange,
                protected: false,
                evidence: format!(
                    "{} has {} lines, above Claude Code's concise-file guidance",
                    source.path, source.line_count
                ),
                rationale: "File length is never deletion evidence by itself; review the content for independently established genericity, duplication, staleness, component scope, or reliable relocation."
                    .to_string(),
                related_sources: Vec::new(),
            });
        }
    }

    for chain in chains {
        if chain.harness == "claude_code" {
            let imported: Vec<_> = chain
                .entries
                .iter()
                .filter(|entry| entry.effective && entry.load_mode == "imported")
                .collect();
            if !imported.is_empty() {
                let imported_bytes: usize = imported.iter().map(|entry| entry.loaded_bytes).sum();
                findings.push(PlacementFinding {
                    severity: "info".to_string(),
                    code: "imported_context_counts".to_string(),
                    category: PlacementCategory::ContextBudget,
                    source: "claude_code".to_string(),
                    line_start: None,
                    line_end: None,
                    action: PlacementAction::Review,
                    destination: PlacementDestination::NoChange,
                    protected: false,
                    evidence: format!(
                        "{} imported source(s) add {imported_bytes} loaded bytes",
                        imported.len()
                    ),
                    rationale: "A Claude @path import is organization, not a token saving: imported bytes remain part of the always-loaded context."
                        .to_string(),
                    related_sources: imported.iter().map(|entry| entry.source.clone()).collect(),
                });
            }
        }
        if chain.harness == "codex"
            && let Some(limit) = chain.project_document_max_bytes
            && (chain.total_loaded_bytes.saturating_mul(10) >= limit.saturating_mul(9)
                || chain.entries.iter().any(|entry| entry.truncated))
        {
            let truncated = chain.entries.iter().any(|entry| entry.truncated);
            let limit_reached = chain.total_loaded_bytes >= limit;
            findings.push(PlacementFinding {
                severity: if truncated || limit_reached {
                    "error"
                } else {
                    "warning"
                }
                .to_string(),
                code: "context_budget_pressure".to_string(),
                category: PlacementCategory::ContextBudget,
                source: "codex".to_string(),
                line_start: None,
                line_end: None,
                action: PlacementAction::Review,
                destination: PlacementDestination::NoChange,
                protected: false,
                evidence: format!(
                    "Codex loads {} of {limit} configured project-document bytes",
                    chain.total_loaded_bytes
                ),
                rationale: "The combined root-to-working-directory byte total signals loading risk, but byte pressure alone never justifies deleting project-specific knowledge."
                    .to_string(),
                related_sources: chain
                    .entries
                    .iter()
                    .filter(|entry| entry.effective)
                    .map(|entry| entry.source.clone())
                    .collect(),
            });
        }
    }
    findings
}

fn directive_signature(text: &str) -> Option<(bool, String)> {
    let clean = strip_markdown_leader(text).to_lowercase();
    let negative_prefixes = [
        "must never ",
        "must not ",
        "never ",
        "do not ",
        "don't ",
        "不得",
        "禁止",
        "不要",
    ];
    for prefix in negative_prefixes {
        if let Some(rest) = clean.strip_prefix(prefix) {
            return Some((false, normalize_guidance(rest)));
        }
    }
    let positive_prefixes = [
        "always ", "must ", "should ", "ensure ", "始终", "必须", "应当",
    ];
    for prefix in positive_prefixes {
        if let Some(rest) = clean.strip_prefix(prefix) {
            return Some((true, normalize_guidance(rest)));
        }
    }
    None
}

fn normalize_guidance(text: &str) -> String {
    strip_markdown_leader(text)
        .chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn whitespace_normalized(text: &str) -> String {
    strip_markdown_leader(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn strip_markdown_leader(text: &str) -> &str {
    let trimmed = text.trim();
    if let Some(rest) = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("+ "))
    {
        return rest.trim();
    }
    let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 {
        let remainder = &trimmed[digits..];
        if let Some(rest) = remainder
            .strip_prefix(". ")
            .or_else(|| remainder.strip_prefix(") "))
        {
            return rest.trim();
        }
    }
    trimmed
}

fn inline_code_spans(text: &str) -> Vec<String> {
    let mut spans = Vec::new();
    let mut remainder = text;
    while let Some(start) = remainder.find('`') {
        let after = &remainder[start + 1..];
        let Some(end) = after.find('`') else {
            break;
        };
        if end > 0 {
            spans.push(after[..end].to_string());
        }
        remainder = &after[end + 1..];
    }
    spans
}

fn markdown_link_targets(text: &str) -> Vec<String> {
    let mut targets = Vec::new();
    let mut remainder = text;
    while let Some(start) = remainder.find("](") {
        let after = &remainder[start + 2..];
        let Some(end) = after.find(')') else {
            break;
        };
        let target = after[..end].trim();
        if !target.is_empty() {
            targets.push(target.to_string());
        }
        remainder = &after[end + 1..];
    }
    targets
}

fn is_skill_reference(value: &str) -> bool {
    let value = value.trim();
    (!value.is_empty()
        && !value.contains(char::is_whitespace)
        && !value.contains('/')
        && !value.contains('.'))
        || value.ends_with("/SKILL.md")
}

fn skill_exists(repository_root: &Path, value: &str) -> bool {
    let value = value.trim();
    if value.ends_with("/SKILL.md") {
        return resolve_repository_path(repository_root, value).is_some();
    }
    [".agents/skills", ".claude/skills", ".codex/skills"]
        .into_iter()
        .any(|root| {
            repository_root
                .join(root)
                .join(value)
                .join("SKILL.md")
                .is_file()
        })
}

fn looks_like_repository_path(value: &str) -> bool {
    let value = value
        .trim()
        .trim_matches(|character| matches!(character, '\'' | '"' | ',' | ';' | ':'));
    !value.is_empty()
        && !value.starts_with('/')
        && !value.starts_with('~')
        && !value.contains("://")
        && !value.contains('$')
        && !value.contains('*')
        && !value.contains(char::is_whitespace)
        && (value.starts_with("./") || value.contains('/'))
}

fn resolve_repository_path(repository_root: &Path, value: &str) -> Option<PathBuf> {
    let raw_value = value
        .trim()
        .trim_matches(|character| matches!(character, '\'' | '"' | ',' | ';'));
    if !looks_like_repository_path(raw_value) {
        return None;
    }
    let value = raw_value.trim_end_matches('/');
    let path = Path::new(value);
    let mut normalized = repository_root.to_path_buf();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(value) => normalized.push(value),
            Component::ParentDir => {
                if normalized == repository_root || !normalized.pop() {
                    return None;
                }
            }
            Component::Prefix(_) | Component::RootDir => return None,
        }
    }
    (normalized.starts_with(repository_root) && normalized.exists()).then_some(normalized)
}

fn is_import_only(line: &str) -> bool {
    line.strip_prefix('@').is_some_and(|path| {
        !path.is_empty()
            && !path.contains(char::is_whitespace)
            && path
                .chars()
                .all(|character| character.is_alphanumeric() || "/._-~".contains(character))
    })
}

fn is_bullet_item(line: &str) -> bool {
    line.starts_with("- ") || line.starts_with("* ") || line.starts_with("+ ")
}

fn is_numbered_item(line: &str) -> bool {
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    digits > 0
        && line[digits..]
            .strip_prefix(". ")
            .or_else(|| line[digits..].strip_prefix(") "))
            .is_some()
}

fn unit_location(unit: &GuidanceUnit) -> String {
    if unit.line_start == unit.line_end {
        format!("{}:{}", unit.source, unit.line_start)
    } else {
        format!("{}:{}-{}", unit.source, unit.line_start, unit.line_end)
    }
}

fn short_evidence(text: &str) -> String {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut output = String::new();
    for character in compact.chars().take(180) {
        output.push(character);
    }
    if compact.chars().count() > 180 {
        output.push('…');
    }
    output
}

fn contains_any(value: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| value.contains(needle))
}
