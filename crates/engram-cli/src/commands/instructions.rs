//! Project instruction diagnostics and proposal-only staging.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use engram_core::{MARKER_END, MARKER_START, PagePath};
use engram_store::{AutoImproveProposalOperation, project_instruction_approval_sha256};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cli::{
    InstructionsApplyArgs, InstructionsArgs, InstructionsCommand, InstructionsDoctorArgs,
    InstructionsProposeArgs,
};
use crate::commands::apply_shared::{ApplyOutcome, apply_atomic_report};
use crate::config::Config;
use crate::http_client::{ServerEndpoint, get_json, post_json, post_json_with_query};
use crate::instruction_placement::{PlacementAction, PlacementDestination, PlacementFinding};
use crate::instruction_steward::DoctorReport;

/// Run the deterministic doctor before config/log/store initialization.
pub fn run_doctor(args: InstructionsDoctorArgs) -> Result<()> {
    let report = DoctorReport::inspect_current_repository()?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        report.print_human();
    }
    Ok(())
}

/// Run state-touching instruction commands through the configured server.
pub async fn run(config: &Config, args: InstructionsArgs) -> Result<()> {
    match args.command {
        InstructionsCommand::Doctor(args) => run_doctor(args),
        InstructionsCommand::Propose(args) => propose(config, *args).await,
        InstructionsCommand::Apply(args) => apply(config, args).await,
    }
}

#[derive(Debug, Deserialize)]
struct PageContent {
    path: String,
    title: Option<String>,
    body: String,
    frontmatter: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct StageRequest {
    workspace: String,
    project: String,
    operation: String,
    logical_target: String,
    target_context_layer: String,
    boundary_kind: String,
    boundary_value: String,
    base_content: String,
    proposed_content: String,
    title: String,
    rationale: String,
    provenance: serde_json::Value,
}

#[derive(Debug, Deserialize, Serialize)]
struct StageResponse {
    proposal_id: Option<String>,
    status: String,
    target_kind: String,
    operation: String,
    logical_target: String,
    target_context_layer: String,
    #[serde(default, skip_serializing_if = "is_false")]
    manual_approval_required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    semantic_assistance: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct ApplyProposalSummary {
    id: String,
    status: String,
    target_kind: String,
    operation: String,
    logical_target: String,
    target_context_layer: String,
}

#[derive(Debug, Deserialize)]
struct ApplyProposalDetail {
    summary: ApplyProposalSummary,
    proposed_content: String,
    base_sha256: Option<String>,
    boundary_kind: Option<String>,
    boundary_value: Option<String>,
    base_content: Option<String>,
    approval_sha256: Option<String>,
    #[serde(default)]
    application: Option<ProjectInstructionApplication>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct ProjectInstructionApplication {
    approval_sha256: String,
    before_sha256: String,
    after_sha256: String,
    outcome: String,
    backup_path: Option<String>,
    proposing_actor: serde_json::Value,
    approving_actor: serde_json::Value,
    applying_actor: serde_json::Value,
    applied_by_author_id: Option<String>,
    applied_at: i64,
}

#[derive(Debug, Serialize)]
struct ApplyRequest {
    expected_approval_sha256: String,
    before_sha256: String,
    after_sha256: String,
    outcome: &'static str,
    backup_path: Option<String>,
}

struct LocalProposal {
    operation: &'static str,
    logical_target: String,
    target_context_layer: String,
    boundary_kind: &'static str,
    boundary_value: String,
    base_content: String,
    proposed_content: String,
    title: String,
    rationale: String,
    provenance: serde_json::Value,
}

async fn propose(config: &Config, args: InstructionsProposeArgs) -> Result<()> {
    let report = DoctorReport::inspect_current_repository()?;
    let project = super::resolve_project_name(args.project.as_deref())?;
    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let local = match (
        args.rule.as_deref(),
        args.finding.as_deref(),
        args.correction.as_deref(),
        args.review_finding.as_deref(),
    ) {
        (Some(rule), None, None, None) => {
            proposal_from_rule(&endpoint, &report, &args, &project, rule).await?
        }
        (None, Some(finding), None, None) => proposal_from_finding(&report, &args, finding)?,
        (None, None, Some(query), None) => {
            proposal_from_semantic_selector(&report, &args, "repeated_project_correction", query)?
        }
        (None, None, None, Some(path)) => {
            if !path.starts_with("_lint/") {
                bail!("durable review findings must come from an explicit _lint/ page");
            }
            proposal_from_semantic_selector(&report, &args, "durable_review_finding", path)?
        }
        _ => bail!("select exactly one of --rule, --finding, --correction, or --review-finding"),
    };
    let route = if args.semantic {
        "/admin/instructions/semantic-proposals"
    } else {
        "/admin/instructions/proposals"
    };
    let response: StageResponse = post_json(
        &endpoint,
        route,
        &StageRequest {
            workspace: args.workspace,
            project,
            operation: local.operation.to_owned(),
            logical_target: local.logical_target,
            target_context_layer: local.target_context_layer,
            boundary_kind: local.boundary_kind.to_owned(),
            boundary_value: local.boundary_value,
            base_content: local.base_content,
            proposed_content: local.proposed_content,
            title: local.title,
            rationale: local.rationale,
            provenance: local.provenance,
        },
    )
    .await
    .context("staging project-instruction proposal")?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&response)?);
    } else if response.status == "rejected" {
        println!(
            "no project-instruction proposal staged for {} ({})",
            response.logical_target, response.target_context_layer
        );
        if let Some(budget) = response.semantic_assistance {
            println!("  semantic assistance budget: {budget}");
        }
    } else {
        let proposal_id = response.proposal_id.as_deref().unwrap_or("none");
        println!(
            "✓ staged {} {} proposal {} for {} ({})",
            response.target_kind,
            response.operation,
            proposal_id,
            response.logical_target,
            response.target_context_layer
        );
        println!("  staged only; no project instruction is active until a later approved apply");
        if let Some(budget) = response.semantic_assistance {
            println!("  semantic assistance budget: {budget}");
        }
    }
    Ok(())
}

fn is_false(value: &bool) -> bool {
    !value
}

fn proposal_from_semantic_selector(
    report: &DoctorReport,
    args: &InstructionsProposeArgs,
    kind: &str,
    source: &str,
) -> Result<LocalProposal> {
    if source.trim().is_empty() {
        bail!("semantic evidence selector cannot be empty");
    }
    let target = args
        .target
        .as_ref()
        .map(|path| path_string(path.as_path()))
        .or_else(|| report.canonical.path.clone())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "instruction doctor could not resolve a canonical target; pass --target"
            )
        })?;
    let (logical_target, base_content) = read_repository_target(report, &target)?;
    Ok(LocalProposal {
        operation: "no_change",
        logical_target,
        target_context_layer: "no_change".into(),
        boundary_kind: "exact_anchor",
        boundary_value: "whole_file_snapshot".into(),
        proposed_content: base_content.clone(),
        base_content,
        title: "Provider-assisted durable instruction review".into(),
        rationale: "Review explicitly selected durable Engram evidence.".into(),
        provenance: serde_json::json!([{
            "kind": kind,
            "source": source,
            "excerpt": source,
            "selection": "explicit_cli",
        }]),
    })
}

async fn apply(config: &Config, args: InstructionsApplyArgs) -> Result<()> {
    let project = super::resolve_project_name(args.project.as_deref())?;
    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let detail: ApplyProposalDetail = get_json(
        &endpoint,
        &format!("/admin/pending-writes/{}", args.id),
        &[
            ("workspace", args.workspace.as_str()),
            ("project", project.as_str()),
        ],
    )
    .await
    .context("fetching approved project-instruction proposal")?;
    if detail.summary.id != args.id {
        bail!("server returned a different project-instruction proposal");
    }
    if detail.summary.target_kind != "project_instruction" {
        bail!("proposal {} does not target a project instruction", args.id);
    }
    if detail.summary.status != "approved" {
        bail!(
            "proposal {} is {}; only approved project-instruction proposals can be applied",
            args.id,
            detail.summary.status
        );
    }
    if let Some(application) = detail.application {
        return print_existing_application(&args, application);
    }

    let operation = AutoImproveProposalOperation::from_str(&detail.summary.operation)
        .context("parsing approved project-instruction operation")?;
    let base_content = detail
        .base_content
        .ok_or_else(|| anyhow::anyhow!("approved proposal has no base content"))?;
    let base_sha256 = detail
        .base_sha256
        .ok_or_else(|| anyhow::anyhow!("approved proposal has no base SHA-256"))?;
    let base_hash = parse_sha256(&base_sha256).context("parsing proposal base SHA-256")?;
    if sha256_bytes(base_content.as_bytes()) != base_hash {
        bail!("approved proposal base content does not match its SHA-256");
    }
    let boundary_kind = detail
        .boundary_kind
        .ok_or_else(|| anyhow::anyhow!("approved proposal has no ownership boundary kind"))?;
    let boundary_value = detail
        .boundary_value
        .ok_or_else(|| anyhow::anyhow!("approved proposal has no ownership boundary value"))?;
    validate_boundary(
        &base_content,
        &detail.proposed_content,
        &boundary_kind,
        &boundary_value,
    )?;

    let approval_sha256 = detail
        .approval_sha256
        .ok_or_else(|| anyhow::anyhow!("approved proposal has no approval binding"))?;
    let approved_hash =
        parse_sha256(&approval_sha256).context("parsing project-instruction approval SHA-256")?;
    let recomputed = project_instruction_approval_sha256(
        operation,
        &detail.summary.logical_target,
        &detail.summary.target_context_layer,
        &base_hash,
        &boundary_kind,
        &boundary_value,
        &detail.proposed_content,
    );
    if recomputed != approved_hash {
        bail!("approved proposal fields do not match its approval binding");
    }

    let report = DoctorReport::inspect_current_repository()?;
    let target = resolve_apply_target(&report, &detail.summary)?;
    let current = read_target(&target)?;
    ensure_expected_base(&current, &base_content, &base_hash)?;

    let proposed_content = detail.proposed_content;
    let closure_base = base_content.clone();
    let closure_proposed = proposed_content.clone();
    let closure_kind = boundary_kind.clone();
    let closure_value = boundary_value.clone();
    let apply_report = apply_atomic_report(&target, move |existing| {
        ensure_expected_base(existing, &closure_base, &base_hash)?;
        validate_boundary(existing, &closure_proposed, &closure_kind, &closure_value)?;
        Ok(closure_proposed)
    })?;
    let before_sha256 = sha256_hex(base_content.as_bytes());
    let after_sha256 = sha256_hex(proposed_content.as_bytes());
    let backup_path = apply_report
        .backup_path
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned());
    let outcome = apply_outcome_name(apply_report.outcome);
    let mut response: serde_json::Value = post_json_with_query(
        &endpoint,
        &format!("/admin/pending-writes/{}/apply", args.id),
        &[
            ("workspace", args.workspace.as_str()),
            ("project", project.as_str()),
        ],
        &ApplyRequest {
            expected_approval_sha256: approval_sha256,
            before_sha256,
            after_sha256,
            outcome,
            backup_path,
        },
    )
    .await
    .context("recording local project-instruction application")?;
    if let Some(object) = response.as_object_mut() {
        object
            .entry("proposal_id")
            .or_insert_with(|| serde_json::json!(args.id));
    }
    print_application(&args, &target, &response)
}

fn print_existing_application(
    args: &InstructionsApplyArgs,
    application: ProjectInstructionApplication,
) -> Result<()> {
    let mut response = serde_json::to_value(application)?;
    if let Some(object) = response.as_object_mut() {
        object.insert("proposal_id".into(), serde_json::json!(args.id));
        object.insert("status".into(), serde_json::json!("applied"));
        object.insert("idempotent".into(), serde_json::json!(true));
    }
    if args.json {
        println!("{}", serde_json::to_string_pretty(&response)?);
    } else {
        println!("✓ proposal {} was already applied", args.id);
    }
    Ok(())
}

fn print_application(
    args: &InstructionsApplyArgs,
    target: &Path,
    response: &serde_json::Value,
) -> Result<()> {
    if args.json {
        println!("{}", serde_json::to_string_pretty(response)?);
    } else {
        let outcome = response["outcome"].as_str().unwrap_or("applied");
        println!(
            "✓ applied proposal {} to {} ({outcome})",
            args.id,
            target.display()
        );
        if let Some(backup) = response["backup_path"].as_str() {
            println!("  recovery backup: {backup}");
        }
    }
    Ok(())
}

async fn proposal_from_rule(
    endpoint: &ServerEndpoint,
    report: &DoctorReport,
    args: &InstructionsProposeArgs,
    project: &str,
    rule: &str,
) -> Result<LocalProposal> {
    if !rule.starts_with("_rules/") {
        bail!("durable instruction evidence must be an explicit _rules/ page");
    }
    let page: PageContent = get_json(
        endpoint,
        "/admin/read-page",
        &[
            ("workspace", args.workspace.as_str()),
            ("project", project),
            ("path", rule),
        ],
    )
    .await
    .with_context(|| format!("reading durable rule {rule}"))?;
    let target = args
        .target
        .as_ref()
        .map(|path| path_string(path.as_path()))
        .or_else(|| report.canonical.path.clone())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "instruction doctor could not resolve a canonical target; pass --target"
            )
        })?;
    let (logical_target, base_content) = read_repository_target(report, &target)?;
    let rule_content = rule_instruction_content(&page.body)?;
    let (operation, proposed_content, target_context_layer) =
        if contains_complete_instruction_blocks(&base_content, &rule_content) {
            ("no_change", base_content.clone(), "no_change".to_owned())
        } else {
            (
                "add",
                append_instruction(&base_content, &rule_content),
                target_layer_for_path(&logical_target).to_owned(),
            )
        };
    let source_hash = sha256_hex(page.body.as_bytes());
    let title = page
        .title
        .or_else(|| first_h1(&page.body))
        .unwrap_or_else(|| page.path.clone());
    Ok(LocalProposal {
        operation,
        logical_target,
        target_context_layer,
        boundary_kind: "exact_anchor",
        boundary_value: "EOF".to_owned(),
        base_content,
        proposed_content,
        title: format!("Promote durable rule: {title}"),
        rationale: format!(
            "Promote the explicitly selected durable rule `{}` into the reviewed project instruction layer.",
            page.path
        ),
        provenance: serde_json::json!([{
            "kind": "durable_rule",
            "source": page.path,
            "source_sha256": source_hash,
            "excerpt": rule_content,
            "selection": "explicit_cli",
            "frontmatter": page.frontmatter,
        }]),
    })
}

fn proposal_from_finding(
    report: &DoctorReport,
    args: &InstructionsProposeArgs,
    finding_code: &str,
) -> Result<LocalProposal> {
    if args.target.is_some() {
        bail!("doctor findings target their diagnosed source; --target is only valid with --rule");
    }
    let matches: Vec<_> = report
        .placement_findings
        .iter()
        .filter(|finding| finding.code == finding_code)
        .filter(|finding| {
            args.source
                .as_deref()
                .is_none_or(|source| finding.source == source)
        })
        .filter(|finding| {
            args.line.is_none_or(|line| {
                finding.line_start.is_some_and(|start| {
                    let end = finding.line_end.unwrap_or(start);
                    (start..=end).contains(&line)
                })
            })
        })
        .collect();
    let finding = match matches.as_slice() {
        [finding] => *finding,
        [] => bail!("doctor finding `{finding_code}` was not found with the selected source/line"),
        many => bail!(
            "doctor finding `{finding_code}` matched {} entries; add --source and --line",
            many.len()
        ),
    };
    proposal_for_selected_finding(report, finding)
}

fn proposal_for_selected_finding(
    report: &DoctorReport,
    finding: &PlacementFinding,
) -> Result<LocalProposal> {
    if !report
        .sources
        .iter()
        .any(|source| source.path == finding.source && source.read_error.is_none())
    {
        bail!(
            "doctor finding `{}` is audit-only and has no readable repository instruction target",
            finding.code
        );
    }
    let (logical_target, base_content) = read_repository_target(report, &finding.source)?;
    let operation = operation_for_finding(finding);
    let proposed_content = if matches!(
        finding.action,
        PlacementAction::Remove | PlacementAction::Move
    ) {
        match (finding.line_start, finding.line_end) {
            (Some(start), end) => remove_lines(&base_content, start, end.unwrap_or(start))?,
            _ => base_content.clone(),
        }
    } else {
        base_content.clone()
    };
    let line_start = finding.line_start;
    let line_end = finding.line_end.or(line_start);
    Ok(LocalProposal {
        operation,
        logical_target: logical_target.clone(),
        target_context_layer: target_layer_for_finding(
            operation,
            finding.destination,
            &logical_target,
        )
        .to_owned(),
        boundary_kind: "exact_anchor",
        boundary_value: match (line_start, line_end) {
            (Some(start), Some(end)) => format!("lines:{start}-{end}"),
            _ => "whole_file_snapshot".to_owned(),
        },
        base_content: base_content.clone(),
        proposed_content,
        title: format!("{} in {}", finding.code, logical_target),
        rationale: finding.rationale.clone(),
        provenance: serde_json::json!([{
            "kind": "doctor_finding",
            "source": finding.source,
            "source_sha256": sha256_hex(base_content.as_bytes()),
            "doctor_schema_version": report.schema_version,
            "finding_code": finding.code,
            "category": finding.category.to_string(),
            "action": finding.action.to_string(),
            "destination": finding.destination.to_string(),
            "line_start": finding.line_start,
            "line_end": finding.line_end,
            "excerpt": finding.evidence,
            "rationale": finding.rationale,
            "related_sources": finding.related_sources,
            "selection": "explicit_cli",
        }]),
    })
}

fn operation_for_finding(finding: &PlacementFinding) -> &'static str {
    match (finding.action, finding.destination) {
        (PlacementAction::Remove, _) => "stale_delete",
        (PlacementAction::Move, PlacementDestination::AgentSkill) => "move_to_skill",
        (PlacementAction::Move, PlacementDestination::PathRules) => "move_to_path_rule",
        (PlacementAction::Move, PlacementDestination::Wiki) => "move_to_wiki",
        (PlacementAction::Reinforce, PlacementDestination::Enforcement) => "move_to_enforcement",
        _ => "no_change",
    }
}

fn target_layer_for_finding(
    operation: &str,
    destination: PlacementDestination,
    logical_target: &str,
) -> &'static str {
    match operation {
        "stale_delete" => target_layer_for_path(logical_target),
        "move_to_skill" => "agent_skill",
        "move_to_path_rule" => "path_rules",
        "move_to_wiki" => "wiki",
        "move_to_enforcement" => "enforcement",
        "no_change" => "no_change",
        _ => match destination {
            PlacementDestination::RootInstructions => "root_instructions",
            PlacementDestination::PathRules => "path_rules",
            PlacementDestination::AgentSkill => "agent_skill",
            PlacementDestination::Wiki => "wiki",
            PlacementDestination::Enforcement => "enforcement",
            PlacementDestination::NoChange => "no_change",
        },
    }
}

fn read_repository_target(report: &DoctorReport, raw: &str) -> Result<(String, String)> {
    let logical = PagePath::new(raw.to_owned())
        .with_context(|| format!("invalid repository-relative instruction target {raw:?}"))?;
    let path = report.repository_root.join(logical.as_str());
    ensure_target_within_repository(&report.repository_root, &path)?;
    let content = match fs::read(&path) {
        Ok(bytes) => String::from_utf8(bytes)
            .with_context(|| format!("instruction target {} is not UTF-8", path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    Ok((logical.to_string(), content))
}

fn ensure_target_within_repository(root: &Path, target: &Path) -> Result<()> {
    if target.strip_prefix(root).ok().is_none_or(|relative| {
        relative
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    }) {
        bail!("instruction target escapes the repository");
    }
    let check = match fs::symlink_metadata(target) {
        Ok(_) => target
            .canonicalize()
            .context("resolving instruction target, including symlinks")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => target
            .parent()
            .unwrap_or(root)
            .canonicalize()
            .context("resolving instruction target parent")?,
        Err(error) => {
            return Err(error).with_context(|| format!("inspecting {}", target.display()));
        }
    };
    let canonical_root = root.canonicalize()?;
    if !check.starts_with(&canonical_root) {
        bail!("instruction target resolves outside the repository");
    }
    Ok(())
}

fn rule_instruction_content(body: &str) -> Result<String> {
    let trimmed = body.trim();
    let content = if trimmed.starts_with("# ") {
        trimmed
            .split_once('\n')
            .map_or("", |(_, remainder)| remainder)
            .trim()
    } else {
        trimmed
    };
    if content.is_empty() {
        bail!("selected durable rule has no instruction content after its title");
    }
    Ok(content.to_owned())
}

fn append_instruction(base: &str, instruction: &str) -> String {
    let mut output = base.to_owned();
    if !output.is_empty() {
        if !output.ends_with('\n') {
            output.push('\n');
        }
        if !output.ends_with("\n\n") {
            output.push('\n');
        }
    }
    output.push_str(instruction.trim());
    output.push('\n');
    output
}

fn remove_lines(content: &str, start: usize, end: usize) -> Result<String> {
    let line_count = if content.is_empty() {
        0
    } else {
        content.split_inclusive('\n').count()
    };
    if start == 0 || end < start || end > line_count {
        bail!("doctor finding has an invalid line range {start}-{end}");
    }
    let mut output = String::with_capacity(content.len());
    for (index, line) in content.split_inclusive('\n').enumerate() {
        let line_number = index + 1;
        if !(start..=end).contains(&line_number) {
            output.push_str(line);
        }
    }
    Ok(output)
}

fn contains_complete_instruction_blocks(target: &str, instruction: &str) -> bool {
    let target_blocks = normalized_instruction_blocks(target);
    let instruction_blocks = normalized_instruction_blocks(instruction);
    !instruction_blocks.is_empty()
        && target_blocks
            .windows(instruction_blocks.len())
            .any(|blocks| blocks == instruction_blocks)
}

fn normalized_instruction_blocks(value: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current = String::new();
    for line in value.lines() {
        if line.trim().is_empty() {
            if !current.is_empty() {
                blocks.push(std::mem::take(&mut current));
            }
            continue;
        }
        for word in line.split_whitespace() {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        blocks.push(current);
    }
    blocks
}

fn target_layer_for_path(path: &str) -> &'static str {
    let target = Path::new(path);
    if target
        .parent()
        .is_none_or(|parent| parent.as_os_str().is_empty())
    {
        "root_instructions"
    } else {
        "path_rules"
    }
}

fn resolve_apply_target(report: &DoctorReport, summary: &ApplyProposalSummary) -> Result<PathBuf> {
    let logical = PagePath::new(summary.logical_target.clone()).with_context(|| {
        format!(
            "invalid approved repository-relative target {:?}",
            summary.logical_target
        )
    })?;
    if summary.target_context_layer == "root_instructions"
        && let Some(canonical) = report.canonical.path.as_deref()
        && canonical != logical.as_str()
    {
        bail!(
            "approved root target {} is no longer the canonical instruction source ({canonical})",
            logical.as_str()
        );
    }
    let target = report.repository_root.join(logical.as_str());
    ensure_target_within_repository(&report.repository_root, &target)?;
    Ok(target)
}

fn read_target(path: &Path) -> Result<String> {
    match fs::read(path) {
        Ok(bytes) => String::from_utf8(bytes)
            .with_context(|| format!("instruction target {} is not UTF-8", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

fn ensure_expected_base(current: &str, base_content: &str, base_hash: &[u8; 32]) -> Result<()> {
    if current != base_content || sha256_bytes(current.as_bytes()) != *base_hash {
        bail!("instruction target changed after proposal staging; refusing to overwrite it");
    }
    Ok(())
}

fn validate_boundary(base: &str, proposed: &str, kind: &str, value: &str) -> Result<()> {
    match kind {
        "exact_anchor" => validate_exact_anchor(base, proposed, value)?,
        "owned_region" => validate_owned_region(base, proposed, value)?,
        other => bail!("unsupported project-instruction ownership boundary {other:?}"),
    }
    validate_routing_block(base, proposed)
}

fn validate_exact_anchor(base: &str, proposed: &str, anchor: &str) -> Result<()> {
    match anchor {
        "EOF" => {
            if !proposed.starts_with(base) {
                bail!("approved EOF change modifies bytes before the exact anchor");
            }
        }
        "whole_file_snapshot" => {}
        lines if lines.starts_with("lines:") => {
            let range = lines.trim_start_matches("lines:");
            let (start, end) = range
                .split_once('-')
                .ok_or_else(|| anyhow::anyhow!("invalid approved line anchor {anchor:?}"))?;
            let start = start
                .parse::<usize>()
                .with_context(|| format!("invalid approved line anchor {anchor:?}"))?;
            let end = end
                .parse::<usize>()
                .with_context(|| format!("invalid approved line anchor {anchor:?}"))?;
            let (start_offset, end_offset) = line_byte_range(base, start, end)?;
            let prefix = &base[..start_offset];
            let suffix = &base[end_offset..];
            if !preserves_disjoint_outside_bytes(proposed, prefix, suffix) {
                bail!("approved line change modifies bytes outside its exact anchor");
            }
        }
        _ => bail!("unsupported exact project-instruction anchor {anchor:?}"),
    }
    Ok(())
}

fn line_byte_range(content: &str, start: usize, end: usize) -> Result<(usize, usize)> {
    if start == 0 || end < start {
        bail!("invalid approved line range {start}-{end}");
    }
    let lines: Vec<_> = content.split_inclusive('\n').collect();
    if end > lines.len() {
        bail!("approved line range {start}-{end} exceeds the target");
    }
    let start_offset = lines[..start - 1].iter().map(|line| line.len()).sum();
    let end_offset = lines[..end].iter().map(|line| line.len()).sum();
    Ok((start_offset, end_offset))
}

fn validate_owned_region(base: &str, proposed: &str, region: &str) -> Result<()> {
    let (start_marker, end_marker) = match region {
        "approved_rules" | "approved-rules" | "engram:approved-rules" => (
            "<!-- engram:approved-rules:start -->",
            "<!-- engram:approved-rules:end -->",
        ),
        _ => bail!("unsupported project-instruction owned region {region:?}"),
    };
    let start = base
        .find(start_marker)
        .ok_or_else(|| anyhow::anyhow!("approved-rules region start marker is missing"))?;
    let body_start = start + start_marker.len();
    let end = base[body_start..]
        .find(end_marker)
        .map(|relative| body_start + relative)
        .ok_or_else(|| anyhow::anyhow!("approved-rules region end marker is missing"))?;
    if !preserves_disjoint_outside_bytes(proposed, &base[..body_start], &base[end..]) {
        bail!("approved change modifies bytes outside the owned project-rules region");
    }
    Ok(())
}

fn preserves_disjoint_outside_bytes(proposed: &str, prefix: &str, suffix: &str) -> bool {
    proposed.len() >= prefix.len().saturating_add(suffix.len())
        && proposed.starts_with(prefix)
        && proposed.ends_with(suffix)
}

fn validate_routing_block(base: &str, proposed: &str) -> Result<()> {
    if routing_block(base) != routing_block(proposed) {
        bail!("approved instruction change would modify the Engram routing block");
    }
    Ok(())
}

fn routing_block(content: &str) -> Option<&str> {
    let start = content.find(MARKER_START)?;
    let after_start = start + MARKER_START.len();
    let end = content[after_start..].find(MARKER_END)? + after_start + MARKER_END.len();
    Some(&content[start..end])
}

fn parse_sha256(value: &str) -> Result<[u8; 32]> {
    if value.len() != 64 {
        bail!("expected 64 hexadecimal characters");
    }
    let mut output = [0_u8; 32];
    for (index, byte) in output.iter_mut().enumerate() {
        let offset = index * 2;
        *byte = u8::from_str_radix(&value[offset..offset + 2], 16)
            .with_context(|| "SHA-256 contains non-hexadecimal characters")?;
    }
    Ok(output)
}

fn sha256_bytes(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn apply_outcome_name(outcome: ApplyOutcome) -> &'static str {
    match outcome {
        ApplyOutcome::Created => "created",
        ApplyOutcome::Updated => "updated",
        ApplyOutcome::NoOp => "no_op",
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn first_h1(body: &str) -> Option<String> {
    body.lines()
        .find_map(|line| line.trim().strip_prefix("# ").map(str::trim))
        .filter(|title| !title.is_empty())
        .map(str::to_owned)
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::{
        contains_complete_instruction_blocks, preserves_disjoint_outside_bytes,
        validate_exact_anchor, validate_owned_region,
    };

    #[test]
    fn complete_instruction_blocks_ignore_line_wrapping() {
        assert!(contains_complete_instruction_blocks(
            "# Rules\n\nKeep SQLite writes behind\nthe single writer actor.\n",
            "Keep SQLite writes behind the single writer actor."
        ));
    }

    #[test]
    fn instruction_substring_inside_larger_statement_is_not_complete() {
        assert!(!contains_complete_instruction_blocks(
            "# Rules\n\nNever Keep SQLite writes behind the single writer actor.\n",
            "Keep SQLite writes behind the single writer actor."
        ));
    }

    #[test]
    fn line_anchor_rejects_overlapping_identical_outside_bytes() {
        let error = validate_exact_anchor("A\nREMOVE\nA\n", "A\n", "lines:2-2")
            .expect_err("the protected suffix cannot overlap the protected prefix");
        assert!(error.to_string().contains("outside its exact anchor"));
    }

    #[test]
    fn outside_byte_guard_rejects_prefix_suffix_overlap() {
        assert!(!preserves_disjoint_outside_bytes("same", "same", "same"));
        assert!(preserves_disjoint_outside_bytes(
            "sameapprovedsame",
            "same",
            "same"
        ));
    }

    #[test]
    fn owned_region_rejects_changes_after_its_end_marker() {
        let base = concat!(
            "before\n",
            "<!-- engram:approved-rules:start -->\n",
            "owned\n",
            "<!-- engram:approved-rules:end -->\n",
            "after\n"
        );
        let proposed = concat!(
            "before\n",
            "<!-- engram:approved-rules:start -->\n",
            "reviewed\n",
            "<!-- engram:approved-rules:end -->\n"
        );
        assert!(validate_owned_region(base, proposed, "approved_rules").is_err());
    }
}
