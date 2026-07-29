//! Project instruction diagnostics and proposal-only staging.

use std::fs;
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use engram_core::PagePath;
use engram_core::routing_skills::MANAGED_MARKER;
use engram_store::{AutoImproveProposalOperation, project_instruction_approval_sha256};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cli::{
    InstructionsApplyArgs, InstructionsArgs, InstructionsCommand, InstructionsDoctorArgs,
    InstructionsProposeArgs,
};
use crate::commands::apply_shared::{
    ApplyOutcome, ApplyReport, apply_atomic_report, rollback_atomic_report,
};
use crate::config::Config;
use crate::http_client::{ServerEndpoint, get_json, post_json, post_json_with_query};
use crate::instruction_placement::{PlacementAction, PlacementDestination, PlacementFinding};
use crate::instruction_steward::{DoctorReport, managed_instruction_regions};

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
    repository_identity_sha256: String,
    base_target_existed: bool,
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
    repository_identity_sha256: Option<String>,
    base_target_existed: Option<bool>,
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

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum LocalApplyFailureKind {
    Conflict,
    Failed,
}

#[derive(Debug, Serialize)]
struct ApplyFailureRequest<'a> {
    expected_approval_sha256: &'a str,
    kind: LocalApplyFailureKind,
    code: &'static str,
    reason: &'a str,
    repair: &'static str,
}

#[derive(Debug)]
struct LocalApplyFailure {
    kind: LocalApplyFailureKind,
    code: &'static str,
    reason: String,
    repair: &'static str,
}

struct LocalApplySuccess {
    target: PathBuf,
    base_content: String,
    proposed_content: String,
    report: ApplyReport,
    receipt_path: Option<PathBuf>,
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
    repository_identity_sha256: String,
    base_target_existed: bool,
}

#[derive(Debug)]
struct ResolvedTargetSnapshot {
    logical_target: String,
    content: String,
    repository_identity_sha256: String,
    existed: bool,
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
            repository_identity_sha256: local.repository_identity_sha256,
            base_target_existed: local.base_target_existed,
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
    let ResolvedTargetSnapshot {
        logical_target,
        content: base_content,
        repository_identity_sha256,
        existed: base_target_existed,
    } = read_repository_target(report, &target)?;
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
        repository_identity_sha256,
        base_target_existed,
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
    let approval_sha256 = detail
        .approval_sha256
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("approved proposal has no approval binding"))?;
    let approved_hash =
        parse_sha256(approval_sha256).context("parsing project-instruction approval SHA-256")?;
    let local = match apply_approved_locally(&args.id, &detail, approved_hash) {
        Ok(local) => local,
        Err(failure) => {
            let audit_reason = bounded_audit_text(&failure.reason, 1024);
            let recorded: serde_json::Value = post_json_with_query(
                &endpoint,
                &format!("/admin/pending-writes/{}/apply-failure", args.id),
                &[
                    ("workspace", args.workspace.as_str()),
                    ("project", project.as_str()),
                ],
                &ApplyFailureRequest {
                    expected_approval_sha256: approval_sha256,
                    kind: failure.kind,
                    code: failure.code,
                    reason: &audit_reason,
                    repair: failure.repair,
                },
            )
            .await
            .with_context(|| {
                format!(
                    "recording fail-closed local apply audit {} ({})",
                    failure.code, failure.reason
                )
            })?;
            let status = recorded["status"].as_str().unwrap_or("failed");
            bail!(
                "{}; local apply recorded {status} ({}) — repair: {}",
                failure.reason,
                failure.code,
                failure.repair
            );
        }
    };
    let before_sha256 = sha256_hex(local.base_content.as_bytes());
    let after_sha256 = sha256_hex(local.proposed_content.as_bytes());
    let backup_path = local
        .report
        .backup_path
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned());
    let outcome = apply_outcome_name(local.report.outcome);
    let mut response: serde_json::Value = post_json_with_query(
        &endpoint,
        &format!("/admin/pending-writes/{}/apply", args.id),
        &[
            ("workspace", args.workspace.as_str()),
            ("project", project.as_str()),
        ],
        &ApplyRequest {
            expected_approval_sha256: approval_sha256.to_owned(),
            before_sha256,
            after_sha256,
            outcome,
            backup_path,
        },
    )
    .await
    .context("recording local project-instruction application")?;
    if let Some(receipt_path) = &local.receipt_path {
        let _ = fs::remove_file(receipt_path);
    }
    if let Some(object) = response.as_object_mut() {
        object
            .entry("proposal_id")
            .or_insert_with(|| serde_json::json!(args.id));
    }
    print_application(&args, &local.target, &response)
}

fn apply_approved_locally(
    proposal_id: &str,
    detail: &ApplyProposalDetail,
    approved_hash: [u8; 32],
) -> std::result::Result<LocalApplySuccess, LocalApplyFailure> {
    let operation =
        AutoImproveProposalOperation::from_str(&detail.summary.operation).map_err(|error| {
            apply_failed(
                "unsupported_operation",
                error.to_string(),
                MANUAL_REVIEW_REPAIR,
            )
        })?;
    ensure_supported_apply_operation(operation).map_err(|error| {
        apply_failed(
            "unsupported_operation",
            error.to_string(),
            MANUAL_REVIEW_REPAIR,
        )
    })?;
    let base_content = detail.base_content.clone().ok_or_else(|| {
        apply_failed(
            "malformed_proposal",
            "approved proposal has no base content",
            RESTAGE_REPAIR,
        )
    })?;
    if operation == AutoImproveProposalOperation::NoChange
        && base_content != detail.proposed_content
    {
        return Err(apply_failed(
            "malformed_proposal",
            "approved no-change proposal does not preserve the exact base content",
            RESTAGE_REPAIR,
        ));
    }
    let base_sha256 = detail.base_sha256.as_deref().ok_or_else(|| {
        apply_failed(
            "malformed_proposal",
            "approved proposal has no base SHA-256",
            RESTAGE_REPAIR,
        )
    })?;
    let base_hash = parse_sha256(base_sha256).map_err(|error| {
        apply_failed(
            "malformed_proposal",
            format!("approved proposal has an invalid base SHA-256: {error}"),
            RESTAGE_REPAIR,
        )
    })?;
    if sha256_bytes(base_content.as_bytes()) != base_hash {
        return Err(apply_failed(
            "malformed_proposal",
            "approved proposal base content does not match its SHA-256",
            RESTAGE_REPAIR,
        ));
    }
    let boundary_kind = detail.boundary_kind.as_deref().ok_or_else(|| {
        apply_failed(
            "malformed_proposal",
            "approved proposal has no ownership boundary kind",
            RESTAGE_REPAIR,
        )
    })?;
    let boundary_value = detail.boundary_value.as_deref().ok_or_else(|| {
        apply_failed(
            "malformed_proposal",
            "approved proposal has no ownership boundary value",
            RESTAGE_REPAIR,
        )
    })?;
    managed_instruction_regions(&base_content)
        .and_then(|_| managed_instruction_regions(&detail.proposed_content))
        .map_err(|error| apply_failed("malformed_markers", error.to_string(), MARKER_REPAIR))?;
    validate_boundary(
        &base_content,
        &detail.proposed_content,
        boundary_kind,
        boundary_value,
    )
    .map_err(|error| {
        let message = error.to_string();
        let code = if message.contains("ambiguous exact anchor") {
            "ambiguous_anchor"
        } else {
            "invalid_ownership_boundary"
        };
        apply_failed(code, message, RESTAGE_REPAIR)
    })?;
    validate_newline_preservation(&base_content, &detail.proposed_content).map_err(|error| {
        apply_failed(
            "unsupported_newline_change",
            error.to_string(),
            RESTAGE_REPAIR,
        )
    })?;

    let recomputed = project_instruction_approval_sha256(
        operation,
        &detail.summary.logical_target,
        &detail.summary.target_context_layer,
        &base_hash,
        boundary_kind,
        boundary_value,
        &detail.proposed_content,
    );
    if recomputed != approved_hash {
        return Err(apply_failed(
            "approval_binding_mismatch",
            "approved proposal fields do not match its approval binding",
            RESTAGE_REPAIR,
        ));
    }

    let expected_repository_identity =
        detail
            .repository_identity_sha256
            .as_deref()
            .ok_or_else(|| {
                apply_failed(
                    "malformed_proposal",
                    "approved proposal has no originating repository identity",
                    RESTAGE_REPAIR,
                )
            })?;
    let base_target_existed = detail.base_target_existed.ok_or_else(|| {
        apply_failed(
            "malformed_proposal",
            "approved proposal has no target-existence metadata",
            RESTAGE_REPAIR,
        )
    })?;
    let report = DoctorReport::inspect_current_repository().map_err(|error| {
        apply_failed(
            "instruction_preflight_failed",
            format!("instruction doctor preflight failed: {error}"),
            MANUAL_REVIEW_REPAIR,
        )
    })?;
    block_unsafe_doctor_findings(&report)?;
    let target_identity =
        repository_target_identity(&report.repository_root, &detail.summary.logical_target)
            .map_err(|error| {
                apply_failed(
                    "repository_identity_failed",
                    error.to_string(),
                    MANUAL_REVIEW_REPAIR,
                )
            })?;
    let approved_identity_for_current_target = repository_identity_sha256(
        &report.repository_root,
        &detail.summary.logical_target,
        &target_identity.canonical_target,
        base_target_existed,
    )
    .map_err(|error| {
        apply_failed(
            "repository_identity_failed",
            error.to_string(),
            MANUAL_REVIEW_REPAIR,
        )
    })?;
    if approved_identity_for_current_target != expected_repository_identity {
        return Err(apply_conflict(
            "different_repository",
            "approved proposal belongs to a different repository or canonical target; refusing local apply",
            RESTAGE_REPAIR,
        ));
    }
    let possible_completed_creation = operation == AutoImproveProposalOperation::Add
        && !base_target_existed
        && target_identity.existed;
    if target_identity.existed != base_target_existed && !possible_completed_creation {
        return Err(apply_conflict(
            "target_changed",
            "instruction target existence changed after proposal staging",
            RESTAGE_REPAIR,
        ));
    }
    ensure_clean_git_operation_state(&report.repository_root)?;
    let target = resolve_apply_target(&report, &detail.summary, &target_identity.canonical_target)
        .map_err(|error| apply_failed("unsafe_target", error.to_string(), MANUAL_REVIEW_REPAIR))?;
    let current_target_existed = target.try_exists().map_err(|error| {
        apply_failed(
            "target_metadata_failed",
            format!("checking instruction target existence failed: {error}"),
            MANUAL_REVIEW_REPAIR,
        )
    })?;
    let current = read_target(&target).map_err(|error| {
        let message = error.to_string();
        let code = if message.contains("not UTF-8") {
            "unsupported_encoding"
        } else {
            "target_read_failed"
        };
        apply_conflict(code, message, RESTAGE_REPAIR)
    })?;
    if current.contains(MANAGED_MARKER) {
        return Err(apply_failed(
            "managed_skill_target",
            "project-instruction apply cannot modify an Engram-managed Skill",
            "use the dedicated Skill installer or uninstaller after manual review",
        ));
    }
    if current == detail.proposed_content {
        if operation == AutoImproveProposalOperation::NoChange {
            ensure_clean_git_target(
                &report.repository_root,
                &detail.summary.logical_target,
                &target,
                false,
            )?;
        }
        let (apply_report, receipt_path) = if base_content == detail.proposed_content {
            (
                ApplyReport {
                    outcome: ApplyOutcome::NoOp,
                    backup_path: None,
                },
                None,
            )
        } else {
            let (apply_report, receipt_path) = recover_unrecorded_apply(
                &report.repository_root,
                proposal_id,
                approved_hash,
                &target,
                &base_content,
                &detail.proposed_content,
                base_target_existed,
                expected_repository_identity,
                &detail.summary.logical_target,
            )
            .map_err(|error| {
                apply_conflict(
                    "target_changed",
                    format!(
                        "target matches the proposal without an authenticated local apply receipt: {error}"
                    ),
                    RESTAGE_REPAIR,
                )
            })?;
            (apply_report, Some(receipt_path))
        };
        return Ok(LocalApplySuccess {
            target,
            base_content,
            proposed_content: detail.proposed_content.clone(),
            report: apply_report,
            receipt_path,
        });
    }
    if current_target_existed != base_target_existed {
        return Err(apply_conflict(
            "target_changed",
            "instruction target existence changed after proposal staging",
            RESTAGE_REPAIR,
        ));
    }
    ensure_expected_base(&current, &base_content, &base_hash)
        .map_err(|error| apply_conflict("target_changed", error.to_string(), RESTAGE_REPAIR))?;
    ensure_clean_git_target(
        &report.repository_root,
        &detail.summary.logical_target,
        &target,
        operation == AutoImproveProposalOperation::Add && !base_target_existed,
    )?;
    prepare_apply_receipt_storage(&report.repository_root).map_err(|error| {
        apply_failed(
            "local_receipt_failed",
            error.to_string(),
            MANUAL_REVIEW_REPAIR,
        )
    })?;
    let final_identity =
        repository_target_identity(&report.repository_root, &detail.summary.logical_target)
            .map_err(|error| {
                apply_conflict(
                    "target_changed",
                    format!("rechecking canonical target identity failed: {error}"),
                    RESTAGE_REPAIR,
                )
            })?;
    if final_identity.sha256 != target_identity.sha256 || final_identity.canonical_target != target
    {
        return Err(apply_conflict(
            "target_changed",
            "instruction target or symlink resolution changed during apply preflight",
            RESTAGE_REPAIR,
        ));
    }
    let receipt_slot = reserve_apply_receipt(&report.repository_root, proposal_id, approved_hash)
        .map_err(|error| {
        apply_failed(
            "local_receipt_failed",
            error.to_string(),
            MANUAL_REVIEW_REPAIR,
        )
    })?;
    let closure_base = base_content.clone();
    let closure_proposed = detail.proposed_content.clone();
    let closure_kind = boundary_kind.to_owned();
    let closure_value = boundary_value.to_owned();
    let apply_result = apply_atomic_report(&target, move |existing| {
        ensure_expected_base(existing, &closure_base, &base_hash)?;
        validate_boundary(existing, &closure_proposed, &closure_kind, &closure_value)?;
        validate_newline_preservation(existing, &closure_proposed)?;
        Ok(closure_proposed)
    });
    let apply_report = match apply_result {
        Ok(report) => report,
        Err(error) => {
            let _ = fs::remove_file(&receipt_slot.path);
            let message = error.to_string();
            return Err(
                if message.contains("changed after proposal staging")
                    || message.contains("changed concurrently")
                    || message.contains("appeared during atomic apply")
                    || message.contains("re-reading")
                {
                    apply_conflict("target_changed", message, RESTAGE_REPAIR)
                } else {
                    apply_failed("filesystem_write_failed", message, MANUAL_REVIEW_REPAIR)
                },
            );
        }
    };
    let post_write_identity_error =
        match repository_target_identity(&report.repository_root, &detail.summary.logical_target) {
            Ok(identity) => {
                let projected = repository_identity_sha256(
                    &report.repository_root,
                    &detail.summary.logical_target,
                    &identity.canonical_target,
                    base_target_existed,
                );
                let bytes_match = fs::read(&target)
                    .is_ok_and(|bytes| bytes == detail.proposed_content.as_bytes());
                if identity.existed
                    && identity.canonical_target == target
                    && projected.is_ok_and(|sha256| sha256 == expected_repository_identity)
                    && bytes_match
                {
                    None
                } else {
                    Some(
                        "logical target, symlink resolution, or bytes changed during write"
                            .to_owned(),
                    )
                }
            }
            Err(error) => Some(format!(
                "canonical target identity recheck failed after write: {error}"
            )),
        };
    if let Some(identity_error) = post_write_identity_error {
        let _ = fs::remove_file(&receipt_slot.path);
        let rollback_detail = match rollback_atomic_report(
            &target,
            detail.proposed_content.as_bytes(),
            base_content.as_bytes(),
            &apply_report,
        ) {
            Ok(_) => "target restored without overwrite".to_owned(),
            Err(error) => format!("rollback retained recovery artifacts: {error}"),
        };
        return Err(apply_conflict(
            "target_changed",
            format!("{identity_error}; {rollback_detail}"),
            RESTAGE_REPAIR,
        ));
    }
    let reserved_receipt_path = receipt_slot.path.clone();
    let receipt_path = match finalize_apply_receipt(
        receipt_slot,
        &report.repository_root,
        proposal_id,
        approved_hash,
        expected_repository_identity,
        &detail.summary.logical_target,
        &target,
        &base_content,
        &detail.proposed_content,
        &apply_report,
    ) {
        Ok(path) => path,
        Err(error) => {
            let _ = fs::remove_file(&reserved_receipt_path);
            match rollback_atomic_report(
                &target,
                detail.proposed_content.as_bytes(),
                base_content.as_bytes(),
                &apply_report,
            ) {
                Ok(_) => {
                    return Err(apply_failed(
                        "local_receipt_failed",
                        format!("{error}; target restored without overwrite"),
                        MANUAL_REVIEW_REPAIR,
                    ));
                }
                Err(rollback_error) => {
                    return Err(apply_conflict(
                        "target_changed",
                        format!(
                            "{error}; rollback could not restore the path after a concurrent change; recovery artifacts were retained: {rollback_error}"
                        ),
                        RESTAGE_REPAIR,
                    ));
                }
            }
        }
    };
    Ok(LocalApplySuccess {
        target,
        base_content,
        proposed_content: detail.proposed_content.clone(),
        report: apply_report,
        receipt_path: Some(receipt_path),
    })
}

const RESTAGE_REPAIR: &str =
    "inspect the checkout, then stage and approve a fresh proposal against the current bytes";
const MANUAL_REVIEW_REPAIR: &str =
    "repair the reported local condition manually, then stage and approve a fresh proposal";
const MARKER_REPAIR: &str = "repair each managed domain into at most one disjoint start/end pair, then restage and reapprove";

fn apply_conflict(
    code: &'static str,
    reason: impl Into<String>,
    repair: &'static str,
) -> LocalApplyFailure {
    LocalApplyFailure {
        kind: LocalApplyFailureKind::Conflict,
        code,
        reason: reason.into(),
        repair,
    }
}

fn apply_failed(
    code: &'static str,
    reason: impl Into<String>,
    repair: &'static str,
) -> LocalApplyFailure {
    LocalApplyFailure {
        kind: LocalApplyFailureKind::Failed,
        code,
        reason: reason.into(),
        repair,
    }
}

fn bounded_audit_text(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let suffix = "…[truncated]";
    let mut end = max_bytes.saturating_sub(suffix.len());
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    format!("{}{suffix}", &value[..end])
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
    let ResolvedTargetSnapshot {
        logical_target,
        content: base_content,
        repository_identity_sha256,
        existed: base_target_existed,
    } = read_repository_target(report, &target)?;
    let rule_content = rule_instruction_content(&page.body)?;
    let (operation, proposed_content, target_context_layer) =
        if contains_complete_instruction_blocks(&base_content, &rule_content) {
            ("no_change", base_content.clone(), "no_change".to_owned())
        } else {
            (
                "add",
                append_instruction(&base_content, &rule_content)?,
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
        repository_identity_sha256,
        base_target_existed,
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
    let ResolvedTargetSnapshot {
        logical_target,
        content: base_content,
        repository_identity_sha256,
        existed: base_target_existed,
    } = read_repository_target(report, &finding.source)?;
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
        repository_identity_sha256,
        base_target_existed,
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

fn read_repository_target(report: &DoctorReport, raw: &str) -> Result<ResolvedTargetSnapshot> {
    read_repository_target_at_root(&report.repository_root, raw, || {})
}

fn read_repository_target_at_root<F>(
    repository_root: &Path,
    raw: &str,
    after_read: F,
) -> Result<ResolvedTargetSnapshot>
where
    F: FnOnce(),
{
    let logical = PagePath::new(raw.to_owned())
        .with_context(|| format!("invalid repository-relative instruction target {raw:?}"))?;
    ensure_not_managed_skill_path(logical.as_str())?;
    let identity = repository_target_identity(repository_root, logical.as_str())?;
    let bytes = if identity.existed {
        fs::read(&identity.canonical_target).with_context(|| {
            format!(
                "reading resolved instruction target {}",
                identity.canonical_target.display()
            )
        })?
    } else {
        match fs::read(&identity.canonical_target) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Ok(_) => bail!("instruction target appeared during proposal staging"),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "checking absent instruction target {}",
                        identity.canonical_target.display()
                    )
                });
            }
        }
    };
    let content = String::from_utf8(bytes).with_context(|| {
        format!(
            "instruction target {} is not UTF-8",
            identity.canonical_target.display()
        )
    })?;
    after_read();
    let final_identity = repository_target_identity(repository_root, logical.as_str())?;
    if final_identity.sha256 != identity.sha256
        || final_identity.canonical_target != identity.canonical_target
        || final_identity.existed != identity.existed
    {
        bail!("instruction target or symlink resolution changed during proposal staging");
    };
    if content.contains(MANAGED_MARKER) {
        bail!("project-instruction proposals cannot modify an Engram-managed Skill");
    }
    Ok(ResolvedTargetSnapshot {
        logical_target: logical.to_string(),
        content,
        repository_identity_sha256: identity.sha256,
        existed: identity.existed,
    })
}

fn ensure_not_managed_skill_path(logical: &str) -> Result<()> {
    let components: Vec<_> = Path::new(logical)
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => value.to_str(),
            _ => None,
        })
        .collect();
    if components.len() >= 2
        && matches!(components[0], ".agents" | ".claude")
        && components[1] == "skills"
    {
        bail!("project-instruction proposals cannot target managed Agent Skill directories");
    }
    Ok(())
}

fn ensure_target_within_repository(root: &Path, target: &Path) -> Result<()> {
    let canonical_root = root.canonicalize()?;
    if target
        .strip_prefix(&canonical_root)
        .ok()
        .is_none_or(|relative| {
            relative
                .components()
                .any(|part| matches!(part, Component::ParentDir))
        })
    {
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

fn append_instruction(base: &str, instruction: &str) -> Result<String> {
    let newline = match newline_style(base)? {
        Some(NewlineStyle::CrLf) => "\r\n",
        Some(NewlineStyle::Lf) | None => "\n",
    };
    let mut output = base.to_owned();
    if !output.is_empty() {
        if !output.ends_with(newline) {
            output.push_str(newline);
        }
        if !output.ends_with(&format!("{newline}{newline}")) {
            output.push_str(newline);
        }
    }
    let normalized_instruction = instruction.replace("\r\n", "\n").replace('\r', "\n");
    if newline == "\r\n" {
        output.push_str(&normalized_instruction.trim().replace('\n', "\r\n"));
    } else {
        output.push_str(normalized_instruction.trim());
    }
    output.push_str(newline);
    Ok(output)
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

fn resolve_apply_target(
    report: &DoctorReport,
    summary: &ApplyProposalSummary,
    canonical_target: &Path,
) -> Result<PathBuf> {
    let logical = PagePath::new(summary.logical_target.clone()).with_context(|| {
        format!(
            "invalid approved repository-relative target {:?}",
            summary.logical_target
        )
    })?;
    ensure_not_managed_skill_path(logical.as_str())?;
    if summary.target_context_layer == "root_instructions"
        && let Some(canonical) = report.canonical.path.as_deref()
        && canonical != logical.as_str()
    {
        bail!(
            "approved root target {} is no longer the canonical instruction source ({canonical})",
            logical.as_str()
        );
    }
    ensure_target_within_repository(&report.repository_root, canonical_target)?;
    Ok(canonical_target.to_path_buf())
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

fn block_unsafe_doctor_findings(
    report: &DoctorReport,
) -> std::result::Result<(), LocalApplyFailure> {
    for finding in &report.findings {
        let (code, repair, conflict) = match finding.code.as_str() {
            "unsafe_instruction_symlink" => ("unsafe_symlink", MANUAL_REVIEW_REPAIR, true),
            "unsupported_instruction_encoding" => {
                ("unsupported_encoding", MANUAL_REVIEW_REPAIR, true)
            }
            "unresolved_claude_import" => ("unresolved_import", MANUAL_REVIEW_REPAIR, false),
            "claude_import_cycle" => ("import_cycle", MANUAL_REVIEW_REPAIR, false),
            "external_or_invalid_claude_import" => ("escaped_import", MANUAL_REVIEW_REPAIR, false),
            "claude_import_depth_exceeded" => {
                ("import_depth_exceeded", MANUAL_REVIEW_REPAIR, false)
            }
            marker if marker.starts_with("routing_marker_") => {
                ("malformed_markers", MARKER_REPAIR, true)
            }
            "instruction_source_metadata_failed" | "instruction_source_read_failed" => {
                ("instruction_preflight_failed", MANUAL_REVIEW_REPAIR, false)
            }
            _ => continue,
        };
        return Err(if conflict {
            apply_conflict(code, finding.message.clone(), repair)
        } else {
            apply_failed(code, finding.message.clone(), repair)
        });
    }
    Ok(())
}

fn ensure_clean_git_operation_state(
    repository_root: &Path,
) -> std::result::Result<(), LocalApplyFailure> {
    let repository = git2::Repository::discover(repository_root).map_err(|error| {
        apply_failed(
            "git_repository_unavailable",
            format!("could not inspect Git repository state: {error}"),
            MANUAL_REVIEW_REPAIR,
        )
    })?;
    let workdir = repository.workdir().ok_or_else(|| {
        apply_failed(
            "bare_git_repository",
            "local instruction apply requires a non-bare Git working tree",
            MANUAL_REVIEW_REPAIR,
        )
    })?;
    let canonical_workdir = workdir.canonicalize().map_err(|error| {
        apply_failed(
            "git_repository_unavailable",
            format!("could not resolve the Git working tree: {error}"),
            MANUAL_REVIEW_REPAIR,
        )
    })?;
    let canonical_root = repository_root.canonicalize().map_err(|error| {
        apply_failed(
            "git_repository_unavailable",
            format!("could not resolve the instruction repository: {error}"),
            MANUAL_REVIEW_REPAIR,
        )
    })?;
    if canonical_workdir != canonical_root {
        return Err(apply_failed(
            "git_repository_mismatch",
            "instruction repository root does not match the Git working tree root",
            MANUAL_REVIEW_REPAIR,
        ));
    }
    if repository.state() != git2::RepositoryState::Clean {
        return Err(apply_conflict(
            "ambiguous_git_state",
            format!(
                "ambiguous Git operation state {:?}; local apply never merges, rebases, or forces",
                repository.state()
            ),
            "finish or abort the current Git operation manually, then restage and reapprove",
        ));
    }
    Ok(())
}

fn ensure_clean_git_target(
    repository_root: &Path,
    logical_target: &str,
    resolved_target: &Path,
    allow_missing_target: bool,
) -> std::result::Result<(), LocalApplyFailure> {
    let repository = git2::Repository::open(repository_root).map_err(|error| {
        apply_failed(
            "git_repository_unavailable",
            format!("could not inspect Git target state: {error}"),
            MANUAL_REVIEW_REPAIR,
        )
    })?;
    let mut targets = vec![PathBuf::from(logical_target)];
    let canonical_root = repository_root.canonicalize().map_err(|error| {
        apply_failed(
            "git_status_failed",
            format!("could not resolve the Git target root: {error}"),
            MANUAL_REVIEW_REPAIR,
        )
    })?;
    let canonical_target = resolved_target
        .canonicalize()
        .unwrap_or_else(|_| resolved_target.to_path_buf());
    if let Ok(relative) = canonical_target.strip_prefix(canonical_root)
        && !targets.iter().any(|target| target == relative)
    {
        targets.push(relative.to_path_buf());
    }
    for target in targets {
        let status = match repository.status_file(&target) {
            Ok(status) => status,
            Err(error) if allow_missing_target && error.code() == git2::ErrorCode::NotFound => {
                continue;
            }
            Err(error) => {
                return Err(apply_failed(
                    "git_status_failed",
                    format!("could not inspect instruction target Git state: {error}"),
                    MANUAL_REVIEW_REPAIR,
                ));
            }
        };
        if status != git2::Status::CURRENT {
            return Err(apply_conflict(
                "dirty_instruction_target",
                format!(
                    "dirty instruction target {} has Git status {:?}; refusing to overwrite it",
                    path_string(&target),
                    status
                ),
                "commit, restore, or otherwise resolve the target's index and worktree changes, then restage and reapprove",
            ));
        }
    }
    Ok(())
}

fn validate_newline_preservation(base: &str, proposed: &str) -> Result<()> {
    let base_style = newline_style(base)?;
    let proposed_style = newline_style(proposed)?;
    if let Some(base_style) = base_style
        && proposed_style.is_some_and(|style| style != base_style)
    {
        bail!("approved instruction change does not preserve the target newline style");
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NewlineStyle {
    Lf,
    CrLf,
}

fn newline_style(content: &str) -> Result<Option<NewlineStyle>> {
    let bytes = content.as_bytes();
    let mut lf = 0usize;
    let mut crlf = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'\r' if bytes.get(index + 1) == Some(&b'\n') => {
                crlf += 1;
                index += 2;
            }
            b'\r' => bail!("instruction target uses unsupported bare-CR newlines"),
            b'\n' => {
                lf += 1;
                index += 1;
            }
            _ => index += 1,
        }
    }
    match (lf, crlf) {
        (0, 0) => Ok(None),
        (0, _) => Ok(Some(NewlineStyle::CrLf)),
        (_, 0) => Ok(Some(NewlineStyle::Lf)),
        _ => bail!("instruction target uses mixed LF and CRLF newlines"),
    }
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
        "whole_file_snapshot" => {
            if base != proposed {
                bail!("approved whole-file snapshot cannot authorize changed content");
            }
        }
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
            let anchored = &base[start_offset..end_offset];
            if !anchored.is_empty() && base.match_indices(anchored).count() != 1 {
                bail!(
                    "ambiguous exact anchor matches more than one location; restage with a unique anchor"
                );
            }
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
    match region {
        "approved_rules" | "approved-rules" | "engram:approved-rules" => {}
        _ => bail!("unsupported project-instruction owned region {region:?}"),
    }
    let base_regions = managed_instruction_regions(base)?;
    let proposed_regions = managed_instruction_regions(proposed)?;
    let base_region = base_regions
        .approved_rules
        .ok_or_else(|| anyhow::anyhow!("approved-rules region is missing"))?;
    let proposed_region = proposed_regions
        .approved_rules
        .ok_or_else(|| anyhow::anyhow!("approved-rules region is missing from proposed content"))?;
    let base_start_marker_end = base_region.start + "<!-- engram:approved-rules:start -->".len();
    let base_end_marker_start = base_region.end - "<!-- engram:approved-rules:end -->".len();
    if !preserves_disjoint_outside_bytes(
        proposed,
        &base[..base_start_marker_end],
        &base[base_end_marker_start..],
    ) || proposed_region.start != base_region.start
    {
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
    let base_regions = managed_instruction_regions(base)?;
    let proposed_regions = managed_instruction_regions(proposed)?;
    let base_block = base_regions.routing.map(|range| &base[range]);
    let proposed_block = proposed_regions.routing.map(|range| &proposed[range]);
    if base_block != proposed_block {
        bail!("approved instruction change would modify the Engram routing block");
    }
    Ok(())
}

fn ensure_supported_apply_operation(operation: AutoImproveProposalOperation) -> Result<()> {
    if matches!(
        operation,
        AutoImproveProposalOperation::Add
            | AutoImproveProposalOperation::Update
            | AutoImproveProposalOperation::StaleDelete
            | AutoImproveProposalOperation::NoChange
    ) {
        return Ok(());
    }
    bail!(
        "{} is not supported by the single-target local apply path",
        operation.as_str()
    )
}

struct RepositoryTargetIdentity {
    sha256: String,
    canonical_target: PathBuf,
    existed: bool,
}

fn repository_target_identity(
    repository_root: &Path,
    logical_target: &str,
) -> Result<RepositoryTargetIdentity> {
    let canonical_root = fs::canonicalize(repository_root).with_context(|| {
        format!(
            "resolving canonical repository root {}",
            repository_root.display()
        )
    })?;
    let target = repository_root.join(logical_target);
    let (canonical_target, existed) = match fs::symlink_metadata(&target) {
        Ok(_) => (
            fs::canonicalize(&target)
                .with_context(|| format!("resolving existing target {}", target.display()))?,
            true,
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = target.parent().unwrap_or(repository_root);
            (
                fs::canonicalize(parent)
                    .with_context(|| format!("resolving target parent {}", parent.display()))?
                    .join(target.file_name().ok_or_else(|| {
                        anyhow::anyhow!("instruction target has no file name: {}", target.display())
                    })?),
                false,
            )
        }
        Err(error) => {
            return Err(error).with_context(|| format!("inspecting target {}", target.display()));
        }
    };
    if !canonical_target.starts_with(&canonical_root) {
        bail!("instruction target resolves outside the canonical repository root");
    }
    let sha256 =
        repository_identity_sha256(&canonical_root, logical_target, &canonical_target, existed)?;
    Ok(RepositoryTargetIdentity {
        sha256,
        canonical_target,
        existed,
    })
}

fn repository_identity_sha256(
    repository_root: &Path,
    logical_target: &str,
    canonical_target: &Path,
    existed: bool,
) -> Result<String> {
    let canonical_root = fs::canonicalize(repository_root).with_context(|| {
        format!(
            "resolving canonical repository root {}",
            repository_root.display()
        )
    })?;
    if !canonical_target.starts_with(&canonical_root) {
        bail!("instruction target resolves outside the canonical repository root");
    }
    let canonical_root = canonical_root.to_str().ok_or_else(|| {
        anyhow::anyhow!(
            "canonical repository root {} is not valid UTF-8",
            canonical_root.display()
        )
    })?;
    let canonical_target = canonical_target.to_str().ok_or_else(|| {
        anyhow::anyhow!(
            "canonical instruction target {} is not valid UTF-8",
            canonical_target.display()
        )
    })?;
    let mut identity = Sha256::new();
    identity.update(canonical_root.as_bytes());
    identity.update([0]);
    identity.update(logical_target.as_bytes());
    identity.update([0]);
    identity.update(canonical_target.as_bytes());
    identity.update([u8::from(existed)]);
    Ok(format!("{:x}", identity.finalize()))
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct ApplyReceiptPayload {
    version: u8,
    proposal_id: String,
    approval_sha256: String,
    repository_identity_sha256: String,
    logical_target: String,
    canonical_target: String,
    before_sha256: String,
    after_sha256: String,
    outcome: String,
    backup_path: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ApplyReceiptEnvelope {
    payload: ApplyReceiptPayload,
    hmac_sha256: String,
}

struct ApplyReceiptSlot {
    path: PathBuf,
    file: fs::File,
}

#[allow(clippy::too_many_arguments)]
fn recover_unrecorded_apply(
    repository_root: &Path,
    proposal_id: &str,
    approval_sha256: [u8; 32],
    target: &Path,
    base: &str,
    proposed: &str,
    base_target_existed: bool,
    repository_identity_sha256: &str,
    logical_target: &str,
) -> Result<(ApplyReport, PathBuf)> {
    let receipt_path = apply_receipt_path(repository_root, proposal_id, approval_sha256)?;
    ensure_regular_nonsymlink(&receipt_path, "local apply receipt")?;
    let bytes = fs::read(&receipt_path)
        .with_context(|| format!("reading local apply receipt {}", receipt_path.display()))?;
    if bytes.len() > 16 * 1024 {
        bail!("local apply receipt is oversized");
    }
    let envelope: ApplyReceiptEnvelope = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing local apply receipt {}", receipt_path.display()))?;
    let key = read_apply_receipt_key(repository_root)?;
    let payload_bytes = serde_json::to_vec(&envelope.payload)?;
    let expected_mac = hmac_sha256(&key, &payload_bytes);
    let actual_mac =
        parse_sha256(&envelope.hmac_sha256).context("local apply receipt has an invalid HMAC")?;
    if !constant_time_eq(&expected_mac, &actual_mac) {
        bail!("local apply receipt authentication failed");
    }
    let canonical_target = canonical_target_string(target)?;
    let expected = ApplyReceiptPayload {
        version: 1,
        proposal_id: proposal_id.to_owned(),
        approval_sha256: hex_sha256(approval_sha256),
        repository_identity_sha256: repository_identity_sha256.to_owned(),
        logical_target: logical_target.to_owned(),
        canonical_target,
        before_sha256: sha256_hex(base.as_bytes()),
        after_sha256: sha256_hex(proposed.as_bytes()),
        outcome: if base_target_existed {
            "updated".to_owned()
        } else {
            "created".to_owned()
        },
        backup_path: envelope.payload.backup_path.clone(),
    };
    if envelope.payload != expected {
        bail!("local apply receipt does not match the approved proposal and canonical target");
    }
    let backup_path = envelope.payload.backup_path.map(PathBuf::from);
    if base_target_existed {
        let backup = backup_path
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("updated local apply receipt has no recovery backup"))?;
        ensure_regular_nonsymlink(backup, "receipt-bound recovery backup")?;
        let canonical_root = repository_root.canonicalize()?;
        if !backup.canonicalize()?.starts_with(canonical_root) {
            bail!("receipt-bound recovery backup resolves outside the repository");
        }
        if fs::read(backup).with_context(|| {
            format!("reading receipt-bound recovery backup {}", backup.display())
        })? != base.as_bytes()
        {
            bail!("receipt-bound recovery backup does not match the approved base bytes");
        }
    } else if backup_path.is_some() {
        bail!("created local apply receipt unexpectedly names a recovery backup");
    }
    Ok((
        ApplyReport {
            outcome: if base_target_existed {
                ApplyOutcome::Updated
            } else {
                ApplyOutcome::Created
            },
            backup_path,
        },
        receipt_path,
    ))
}

#[allow(clippy::too_many_arguments)]
fn finalize_apply_receipt(
    mut slot: ApplyReceiptSlot,
    repository_root: &Path,
    proposal_id: &str,
    approval_sha256: [u8; 32],
    repository_identity_sha256: &str,
    logical_target: &str,
    target: &Path,
    base: &str,
    proposed: &str,
    report: &ApplyReport,
) -> Result<PathBuf> {
    if std::env::var_os("ENGRAM_TEST_FAIL_RECEIPT_FINALIZE").is_some() {
        bail!("injected local receipt finalization failure");
    }
    let payload = ApplyReceiptPayload {
        version: 1,
        proposal_id: proposal_id.to_owned(),
        approval_sha256: hex_sha256(approval_sha256),
        repository_identity_sha256: repository_identity_sha256.to_owned(),
        logical_target: logical_target.to_owned(),
        canonical_target: canonical_target_string(target)?,
        before_sha256: sha256_hex(base.as_bytes()),
        after_sha256: sha256_hex(proposed.as_bytes()),
        outcome: apply_outcome_name(report.outcome).to_owned(),
        backup_path: report
            .backup_path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned()),
    };
    let key = read_apply_receipt_key(repository_root)?;
    let payload_bytes = serde_json::to_vec(&payload)?;
    let envelope = ApplyReceiptEnvelope {
        hmac_sha256: hex_sha256(hmac_sha256(&key, &payload_bytes)),
        payload,
    };
    let bytes = serde_json::to_vec(&envelope)?;
    slot.file
        .write_all(&bytes)
        .context("writing local apply receipt")?;
    slot.file.sync_data().context("fsync local apply receipt")?;
    Ok(slot.path)
}

fn reserve_apply_receipt(
    repository_root: &Path,
    proposal_id: &str,
    approval_sha256: [u8; 32],
) -> Result<ApplyReceiptSlot> {
    let path = apply_receipt_path(repository_root, proposal_id, approval_sha256)?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(&path)
        .with_context(|| format!("reserving local apply receipt {}", path.display()))?;
    Ok(ApplyReceiptSlot { path, file })
}

fn prepare_apply_receipt_storage(repository_root: &Path) -> Result<()> {
    let directory = apply_receipt_directory(repository_root)?;
    fs::create_dir_all(&directory).with_context(|| {
        format!(
            "creating local apply receipt directory {}",
            directory.display()
        )
    })?;
    let metadata = fs::symlink_metadata(&directory)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("local apply receipt storage must be a real directory, not a symlink");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    }
    let key_path = directory.join("hmac-key");
    if key_path.try_exists()? {
        let _ = read_apply_receipt_key(repository_root)?;
        return Ok(());
    }
    let mut key = [0_u8; 32];
    getrandom::fill(&mut key)
        .map_err(|error| anyhow::anyhow!("generating local apply receipt key: {error}"))?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(&key_path) {
        Ok(mut file) => {
            file.write_all(&key)
                .context("writing local apply receipt key")?;
            file.sync_data().context("fsync local apply receipt key")?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = read_apply_receipt_key(repository_root)?;
        }
        Err(error) => return Err(error).context("creating local apply receipt key"),
    }
    Ok(())
}

fn read_apply_receipt_key(repository_root: &Path) -> Result<[u8; 32]> {
    let key_path = apply_receipt_directory(repository_root)?.join("hmac-key");
    ensure_regular_nonsymlink(&key_path, "local apply receipt key")?;
    let bytes = fs::read(&key_path)
        .with_context(|| format!("reading local apply receipt key {}", key_path.display()))?;
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("local apply receipt key must contain exactly 32 bytes"))
}

fn apply_receipt_path(
    repository_root: &Path,
    proposal_id: &str,
    approval_sha256: [u8; 32],
) -> Result<PathBuf> {
    let name = format!(
        "{}-{}.json",
        sha256_hex(proposal_id.as_bytes()),
        hex_sha256(approval_sha256)
    );
    Ok(apply_receipt_directory(repository_root)?.join(name))
}

fn apply_receipt_directory(repository_root: &Path) -> Result<PathBuf> {
    let repository = git2::Repository::open(repository_root)
        .context("opening Git repository for local apply receipt")?;
    Ok(repository.path().join("engram-local-apply"))
}

fn ensure_regular_nonsymlink(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspecting {label} {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("{label} must be a regular non-symlink file");
    }
    Ok(())
}

fn canonical_target_string(target: &Path) -> Result<String> {
    let canonical = fs::canonicalize(target)
        .with_context(|| format!("resolving applied target {}", target.display()))?;
    canonical
        .to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow::anyhow!("canonical applied target is not valid UTF-8"))
}

fn hex_sha256(bytes: [u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hmac_sha256(key: &[u8; 32], message: &[u8]) -> [u8; 32] {
    let mut inner_pad = [0x36_u8; 64];
    let mut outer_pad = [0x5c_u8; 64];
    for index in 0..key.len() {
        inner_pad[index] ^= key[index];
        outer_pad[index] ^= key[index];
    }
    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(message);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner);
    outer.finalize().into()
}

fn constant_time_eq(left: &[u8; 32], right: &[u8; 32]) -> bool {
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
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
    #[cfg(windows)]
    use super::ensure_target_within_repository;
    #[cfg(unix)]
    use super::read_repository_target_at_root;
    use super::{
        ApplyOutcome, ApplyReport, bounded_audit_text, contains_complete_instruction_blocks,
        finalize_apply_receipt, prepare_apply_receipt_storage, preserves_disjoint_outside_bytes,
        recover_unrecorded_apply, reserve_apply_receipt, validate_exact_anchor,
        validate_owned_region,
    };
    use std::fs;

    #[cfg(windows)]
    #[test]
    fn canonical_windows_target_is_accepted_inside_noncanonical_root() {
        let repository = tempfile::tempdir().unwrap();
        let target = repository.path().join("AGENTS.md");
        fs::write(&target, "# Instructions\n").unwrap();

        ensure_target_within_repository(repository.path(), &target.canonicalize().unwrap())
            .unwrap();
    }

    #[test]
    fn audit_reason_is_utf8_safe_and_server_bounded() {
        let reason = "路径".repeat(600);
        let bounded = bounded_audit_text(&reason, 1024);
        assert!(bounded.len() <= 1024);
        assert!(bounded.ends_with("…[truncated]"));
    }

    #[test]
    fn authenticated_receipt_recovers_only_the_bound_update() {
        let repository = tempfile::tempdir().unwrap();
        git2::Repository::init(repository.path()).unwrap();
        let target = repository.path().join("AGENTS.md");
        let backup = repository.path().join("recovery");
        let base = "# Base\n";
        let proposed = "# Base\n\nApproved.\n";
        fs::write(&target, proposed).unwrap();
        fs::write(&backup, base).unwrap();
        let approval = [7_u8; 32];
        let repository_identity = "a".repeat(64);
        prepare_apply_receipt_storage(repository.path()).unwrap();
        let report = ApplyReport {
            outcome: ApplyOutcome::Updated,
            backup_path: Some(backup.clone()),
        };
        let slot = reserve_apply_receipt(repository.path(), "proposal", approval).unwrap();
        let receipt = finalize_apply_receipt(
            slot,
            repository.path(),
            "proposal",
            approval,
            &repository_identity,
            "AGENTS.md",
            &target,
            base,
            proposed,
            &report,
        )
        .unwrap();
        let (recovered, recovered_receipt) = recover_unrecorded_apply(
            repository.path(),
            "proposal",
            approval,
            &target,
            base,
            proposed,
            true,
            &repository_identity,
            "AGENTS.md",
        )
        .unwrap();
        assert_eq!(recovered, report);
        assert_eq!(recovered_receipt, receipt);
    }

    #[cfg(unix)]
    #[test]
    fn receipt_storage_rejects_a_symlink() {
        use std::os::unix::fs::symlink;

        let repository = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let git = git2::Repository::init(repository.path()).unwrap();
        symlink(external.path(), git.path().join("engram-local-apply")).unwrap();
        let error = prepare_apply_receipt_storage(repository.path()).unwrap_err();
        assert!(error.to_string().contains("not a symlink"));
    }

    #[cfg(unix)]
    #[test]
    fn staging_snapshot_rejects_symlink_retarget_after_read() {
        use std::os::unix::fs::symlink;

        let repository = tempfile::tempdir().unwrap();
        fs::write(repository.path().join("one.md"), "same\n").unwrap();
        fs::write(repository.path().join("two.md"), "same\n").unwrap();
        let logical = repository.path().join("AGENTS.md");
        symlink("one.md", &logical).unwrap();
        let retarget = logical.clone();
        let error = read_repository_target_at_root(repository.path(), "AGENTS.md", move || {
            fs::remove_file(&retarget).unwrap();
            symlink("two.md", &retarget).unwrap();
        })
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("changed during proposal staging")
        );
    }

    #[cfg(unix)]
    #[test]
    fn staging_snapshot_rejects_safe_to_external_symlink_retarget() {
        use std::os::unix::fs::symlink;

        let repository = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        fs::write(repository.path().join("inside.md"), "inside\n").unwrap();
        fs::write(external.path().join("outside.md"), "outside\n").unwrap();
        let logical = repository.path().join("AGENTS.md");
        symlink("inside.md", &logical).unwrap();
        let outside = external.path().join("outside.md");
        let retarget = logical.clone();
        let error = read_repository_target_at_root(repository.path(), "AGENTS.md", move || {
            fs::remove_file(&retarget).unwrap();
            symlink(&outside, &retarget).unwrap();
        })
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("outside the canonical repository")
        );
    }

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
