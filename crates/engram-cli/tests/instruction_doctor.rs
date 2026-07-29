//! Black-box acceptance tests for `engram instructions doctor`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

use engram_core::{MARKER_END, MARKER_START, full_block};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_engram")
}

fn init_repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    init_repo_at(repo.path());
    repo
}

fn init_repo_at(path: &Path) {
    fs::create_dir_all(path).unwrap();
    let output = Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(path)
        .output()
        .unwrap();
    assert!(output.status.success());
}

fn run_doctor(project: &Path, home: &Path, extra_args: &[&str]) -> Output {
    let mut command = Command::new(bin());
    command
        .args(["instructions", "doctor", "--json"])
        .args(extra_args)
        .current_dir(project)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("ENGRAM_DATA_DIR", home.join("engram-data"));
    command.output().unwrap()
}

fn assert_success(output: Output) -> Value {
    assert!(
        output.status.success(),
        "command failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, path: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        let mut entries: Vec<_> = fs::read_dir(path).unwrap().flatten().collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let entry_path = entry.path();
            let relative = entry_path.strip_prefix(root).unwrap().to_path_buf();
            let metadata = fs::symlink_metadata(&entry_path).unwrap();
            if metadata.file_type().is_symlink() {
                let mut bytes = b"symlink:".to_vec();
                bytes.extend_from_slice(
                    fs::read_link(&entry_path)
                        .unwrap()
                        .as_os_str()
                        .as_encoded_bytes(),
                );
                out.insert(relative, bytes);
            } else if metadata.is_dir() {
                out.insert(relative.clone(), b"directory".to_vec());
                visit(root, &entry_path, out);
            } else {
                out.insert(relative, fs::read(&entry_path).unwrap());
            }
        }
    }

    let mut out = BTreeMap::new();
    visit(root, root, &mut out);
    out
}

fn doctor_without_writes(project: &Path, home: &Path) -> Value {
    doctor_without_writes_from(project, project, home)
}

fn doctor_without_writes_from(repository: &Path, working_directory: &Path, home: &Path) -> Value {
    let project_before = snapshot(repository);
    let home_before = snapshot(home);
    let report = assert_success(run_doctor(working_directory, home, &[]));
    assert_eq!(
        snapshot(repository),
        project_before,
        "doctor mutated repository"
    );
    assert_eq!(
        snapshot(home),
        home_before,
        "doctor mutated home or Engram state"
    );
    assert!(!home.join("engram-data").exists());
    report
}

#[test]
fn claude_only_repo_reports_canonical_source_and_writes_nothing() {
    let project = init_repo();
    let home = tempfile::tempdir().unwrap();
    let content = "# Claude\n\nUse cargo test.\n";
    fs::write(project.path().join("CLAUDE.md"), content).unwrap();

    let report = doctor_without_writes(project.path(), home.path());

    assert_eq!(report["schema_version"], 2);
    assert_eq!(report["read_only"], true);
    assert_eq!(report["canonical"]["path"], "CLAUDE.md");
    assert_eq!(report["canonical"]["basis"], "single_source");

    let source = report["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|source| source["path"] == "CLAUDE.md")
        .unwrap();
    assert_eq!(source["line_count"], 3);
    assert_eq!(source["byte_count"], content.len());
    assert_eq!(source["estimated_tokens"], content.len().div_ceil(4));
    assert_eq!(source["marker_health"]["status"], "absent");
    assert_eq!(source["routing_asset_drift"]["status"], "not_managed");

    let claude = report["chains"]
        .as_array()
        .unwrap()
        .iter()
        .find(|chain| chain["harness"] == "claude_code")
        .unwrap();
    assert_eq!(claude["support"], "formal");
    assert_eq!(claude["entries"][0]["source"], "CLAUDE.md");
    assert_eq!(claude["entries"][0]["classification"], "canonical");
    assert_eq!(claude["entries"][0]["load_mode"], "startup");
}

#[test]
fn codex_only_repo_reports_root_source_and_writes_nothing() {
    let project = init_repo();
    let home = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("AGENTS.md"),
        "# Agents\n\nRun cargo test.\n",
    )
    .unwrap();

    let report = doctor_without_writes(project.path(), home.path());

    assert_eq!(report["canonical"]["path"], "AGENTS.md");
    assert_eq!(report["canonical"]["basis"], "single_source");
    let codex = report["chains"]
        .as_array()
        .unwrap()
        .iter()
        .find(|chain| chain["harness"] == "codex")
        .unwrap();
    assert_eq!(codex["support"], "formal");
    assert_eq!(codex["entries"][0]["source"], "AGENTS.md");
    assert_eq!(codex["entries"][0]["classification"], "canonical");
    assert_eq!(codex["entries"][0]["load_mode"], "startup");
    assert_eq!(codex["entries"][0]["order"], 1);
}

#[test]
fn global_user_instruction_files_are_outside_the_project_inventory() {
    let home = tempfile::tempdir().unwrap();
    let project = home.path().join("project");
    init_repo_at(&project);
    fs::create_dir_all(home.path().join(".claude")).unwrap();
    fs::create_dir_all(home.path().join(".codex")).unwrap();
    fs::write(
        home.path().join(".claude/CLAUDE.md"),
        "# Private Claude preferences\n",
    )
    .unwrap();
    fs::write(
        home.path().join(".codex/AGENTS.md"),
        "# Private Codex preferences\n",
    )
    .unwrap();
    fs::write(
        home.path().join(".codex/config.toml"),
        "project_doc_max_bytes = 12345\n",
    )
    .unwrap();
    fs::write(project.join("AGENTS.md"), "# Project rules\n").unwrap();

    let report = doctor_without_writes(&project, home.path());

    assert!(report["sources"].as_array().unwrap().iter().all(|source| {
        let path = source["path"].as_str().unwrap();
        source["scope"] != "user"
            && !path.ends_with("/.claude/CLAUDE.md")
            && !path.ends_with("/.codex/AGENTS.md")
    }));
    let codex = report["chains"]
        .as_array()
        .unwrap()
        .iter()
        .find(|chain| chain["harness"] == "codex")
        .unwrap();
    assert_eq!(codex["project_document_max_bytes"], 12345);
    assert_eq!(codex["entries"].as_array().unwrap().len(), 1);
    assert_eq!(codex["entries"][0]["source"], "AGENTS.md");
}

#[test]
fn claude_import_makes_agents_canonical_without_hiding_claude_adapter() {
    let project = init_repo();
    let home = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("CLAUDE.md"),
        "@AGENTS.md\n\n## Claude Code\n\nUse plan mode for billing.\n",
    )
    .unwrap();
    fs::write(
        project.path().join("AGENTS.md"),
        "# Shared rules\n\nRun cargo test.\n",
    )
    .unwrap();

    let report = doctor_without_writes(project.path(), home.path());

    assert_eq!(report["canonical"]["path"], "AGENTS.md");
    assert_eq!(report["canonical"]["basis"], "claude_import");
    let claude = report["chains"]
        .as_array()
        .unwrap()
        .iter()
        .find(|chain| chain["harness"] == "claude_code")
        .unwrap();
    assert_eq!(claude["entries"][0]["source"], "CLAUDE.md");
    assert_eq!(claude["entries"][0]["classification"], "adapter");
    assert_eq!(claude["entries"][1]["source"], "AGENTS.md");
    assert_eq!(claude["entries"][1]["classification"], "canonical");
    assert_eq!(claude["entries"][1]["load_mode"], "imported");

    let codex = report["chains"]
        .as_array()
        .unwrap()
        .iter()
        .find(|chain| chain["harness"] == "codex")
        .unwrap();
    assert_eq!(codex["entries"][0]["source"], "AGENTS.md");
    assert_eq!(codex["entries"][0]["classification"], "canonical");
}

#[test]
fn dot_claude_project_file_can_import_the_shared_agents_source() {
    let project = init_repo();
    let home = tempfile::tempdir().unwrap();
    fs::create_dir_all(project.path().join(".claude")).unwrap();
    fs::write(
        project.path().join(".claude/CLAUDE.md"),
        "@../AGENTS.md\n\n## Claude Code\n\nUse plan mode for billing.\n",
    )
    .unwrap();
    fs::write(
        project.path().join("AGENTS.md"),
        "# Shared rules\n\nRun cargo test.\n",
    )
    .unwrap();

    let report = doctor_without_writes(project.path(), home.path());

    assert_eq!(report["canonical"]["path"], "AGENTS.md");
    assert_eq!(report["canonical"]["basis"], "claude_import");
    let claude = report["chains"]
        .as_array()
        .unwrap()
        .iter()
        .find(|chain| chain["harness"] == "claude_code")
        .unwrap();
    assert_eq!(claude["entries"][0]["source"], ".claude/CLAUDE.md");
    assert_eq!(claude["entries"][0]["classification"], "adapter");
    assert_eq!(claude["entries"][1]["source"], "AGENTS.md");
    assert_eq!(claude["entries"][1]["classification"], "canonical");
}

#[test]
fn claude_imports_stop_after_the_documented_five_hops() {
    let project = init_repo();
    let home = tempfile::tempdir().unwrap();
    fs::create_dir_all(project.path().join("rules")).unwrap();
    fs::write(project.path().join("CLAUDE.md"), "@rules/one.md\n").unwrap();
    for (name, next) in [
        ("one", "two"),
        ("two", "three"),
        ("three", "four"),
        ("four", "five"),
        ("five", "six"),
    ] {
        fs::write(
            project.path().join(format!("rules/{name}.md")),
            format!("@{next}.md\n"),
        )
        .unwrap();
    }
    fs::write(project.path().join("rules/six.md"), "# Too deep\n").unwrap();

    let report = doctor_without_writes(project.path(), home.path());
    let claude = report["chains"]
        .as_array()
        .unwrap()
        .iter()
        .find(|chain| chain["harness"] == "claude_code")
        .unwrap();
    assert!(
        claude["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| { entry["source"] == "rules/five.md" && entry["effective"] == true })
    );
    assert!(
        claude["entries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["source"] != "rules/six.md")
    );
    assert!(
        report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| {
                finding["code"] == "claude_import_depth_exceeded"
                    && finding["sources"][0] == "rules/five.md"
            })
    );
}

#[test]
fn independent_root_files_remain_tool_specific_and_ambiguous() {
    let project = init_repo();
    let home = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("CLAUDE.md"),
        "# Claude rules\n\nUse the Claude browser integration.\n",
    )
    .unwrap();
    fs::write(
        project.path().join("AGENTS.md"),
        "# Codex rules\n\nRun cargo nextest before commits.\n",
    )
    .unwrap();

    let report = doctor_without_writes(project.path(), home.path());

    assert!(report["canonical"]["path"].is_null());
    assert_eq!(
        report["canonical"]["basis"],
        "ambiguous_independent_sources"
    );
    for harness in ["claude_code", "codex"] {
        let chain = report["chains"]
            .as_array()
            .unwrap()
            .iter()
            .find(|chain| chain["harness"] == harness)
            .unwrap();
        assert_eq!(chain["entries"][0]["classification"], "tool_specific");
    }
    assert!(
        report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| {
                finding["code"] == "independent_instruction_sources"
                    && finding["severity"] == "warning"
            })
    );
}

#[test]
fn near_duplicate_root_files_are_reported_as_split_brain() {
    let project = init_repo();
    let home = tempfile::tempdir().unwrap();
    let shared = "# Project rules\n\n- Run cargo fmt before commit.\n- Run cargo test before commit.\n- Keep database writes serialized.\n- Preserve user changes in dirty worktrees.\n- Never commit secrets or credentials.\n";
    fs::write(project.path().join("CLAUDE.md"), shared).unwrap();
    fs::write(
        project.path().join("AGENTS.md"),
        format!("{shared}- Prefer focused regression tests.\n"),
    )
    .unwrap();

    let report = doctor_without_writes(project.path(), home.path());

    assert!(report["canonical"]["path"].is_null());
    assert_eq!(
        report["canonical"]["basis"],
        "ambiguous_near_duplicate_sources"
    );
    assert!(
        report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| {
                finding["code"] == "near_duplicate_instruction_sources"
                    && finding["message"].as_str().unwrap().contains("similarity")
            })
    );
}

#[test]
fn near_duplicate_detection_is_not_limited_to_space_delimited_text() {
    let project = init_repo();
    let home = tempfile::tempdir().unwrap();
    let shared = "# 项目规则\n\n提交前运行格式检查和全部测试。数据库写入必须经过单一写入队列。并行会话不得覆盖其他人的未提交修改。任何凭据、令牌和密码都不得写入版本库。\n";
    fs::write(project.path().join("CLAUDE.md"), shared).unwrap();
    fs::write(
        project.path().join("AGENTS.md"),
        format!("{shared}修复缺陷时增加针对性的回归测试。\n"),
    )
    .unwrap();

    let report = doctor_without_writes(project.path(), home.path());

    assert_eq!(
        report["canonical"]["basis"],
        "ambiguous_near_duplicate_sources"
    );
}

#[test]
fn explicit_project_canonical_configuration_wins_over_inference() {
    let project = init_repo();
    let home = tempfile::tempdir().unwrap();
    fs::write(project.path().join("CLAUDE.md"), "# Shared policy\n").unwrap();
    fs::write(project.path().join("AGENTS.md"), "# Codex-only policy\n").unwrap();
    fs::write(
        project.path().join(".engram.toml"),
        "[instructions]\ncanonical = \"CLAUDE.md\"\n",
    )
    .unwrap();

    let report = doctor_without_writes(project.path(), home.path());

    assert_eq!(report["canonical"]["path"], "CLAUDE.md");
    assert_eq!(report["canonical"]["basis"], "explicit_config");
    let claude = report["chains"]
        .as_array()
        .unwrap()
        .iter()
        .find(|chain| chain["harness"] == "claude_code")
        .unwrap();
    assert_eq!(claude["entries"][0]["classification"], "canonical");
    let codex = report["chains"]
        .as_array()
        .unwrap()
        .iter()
        .find(|chain| chain["harness"] == "codex")
        .unwrap();
    assert_eq!(codex["entries"][0]["classification"], "tool_specific");
}

#[test]
fn explicit_canonical_classification_survives_codex_override_precedence() {
    let project = init_repo();
    let home = tempfile::tempdir().unwrap();
    fs::write(project.path().join("AGENTS.md"), "# Canonical policy\n").unwrap();
    fs::write(
        project.path().join("AGENTS.override.md"),
        "# Temporary Codex override\n",
    )
    .unwrap();
    fs::write(
        project.path().join(".engram.toml"),
        "[instructions]\ncanonical = \"AGENTS.md\"\n",
    )
    .unwrap();

    let report = doctor_without_writes(project.path(), home.path());
    let codex = report["chains"]
        .as_array()
        .unwrap()
        .iter()
        .find(|chain| chain["harness"] == "codex")
        .unwrap();
    let canonical = codex["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["source"] == "AGENTS.md")
        .unwrap();
    assert_eq!(canonical["classification"], "canonical");
    assert_eq!(canonical["load_mode"], "shadowed");
    assert_eq!(canonical["effective"], false);
    let override_entry = codex["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["source"] == "AGENTS.override.md")
        .unwrap();
    assert_eq!(override_entry["classification"], "override");
    assert_eq!(override_entry["effective"], true);
}

#[cfg(unix)]
#[test]
fn safe_symlink_is_reported_as_adapter_to_canonical_source() {
    use std::os::unix::fs::symlink;

    let project = init_repo();
    let home = tempfile::tempdir().unwrap();
    let content = "# Shared rules\n";
    fs::write(project.path().join("AGENTS.md"), content).unwrap();
    symlink("AGENTS.md", project.path().join("CLAUDE.md")).unwrap();

    let report = doctor_without_writes(project.path(), home.path());

    assert_eq!(report["canonical"]["path"], "AGENTS.md");
    assert_eq!(report["canonical"]["basis"], "safe_symlink");
    let claude_source = report["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|source| source["path"] == "CLAUDE.md")
        .unwrap();
    assert_eq!(claude_source["symlink"]["target"], "AGENTS.md");
    assert_eq!(claude_source["symlink"]["safe"], true);

    let claude = report["chains"]
        .as_array()
        .unwrap()
        .iter()
        .find(|chain| chain["harness"] == "claude_code")
        .unwrap();
    assert_eq!(claude["entries"][0]["classification"], "adapter");
    assert_eq!(claude["entries"][1]["source"], "AGENTS.md");
    assert_eq!(claude["entries"][1]["load_mode"], "symlink_target");
    assert_eq!(claude["entries"][1]["effective"], false);
    assert_eq!(claude["entries"][1]["loaded_bytes"], 0);
    assert_eq!(claude["total_loaded_bytes"], content.len());
}

#[cfg(unix)]
#[test]
fn reverse_safe_symlink_keeps_claude_as_canonical_source() {
    use std::os::unix::fs::symlink;

    let project = init_repo();
    let home = tempfile::tempdir().unwrap();
    fs::write(project.path().join("CLAUDE.md"), "# Shared rules\n").unwrap();
    symlink("CLAUDE.md", project.path().join("AGENTS.md")).unwrap();

    let report = doctor_without_writes(project.path(), home.path());

    assert_eq!(report["canonical"]["path"], "CLAUDE.md");
    assert_eq!(report["canonical"]["basis"], "safe_symlink");
    let codex = report["chains"]
        .as_array()
        .unwrap()
        .iter()
        .find(|chain| chain["harness"] == "codex")
        .unwrap();
    assert_eq!(codex["entries"][0]["source"], "AGENTS.md");
    assert_eq!(codex["entries"][0]["classification"], "adapter");
    let target = codex["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["source"] == "CLAUDE.md")
        .unwrap();
    assert_eq!(target["load_mode"], "symlink_target");
    assert_eq!(target["effective"], false);
}

#[test]
fn nested_rules_and_codex_overrides_report_scope_and_precedence() {
    let project = init_repo();
    let home = tempfile::tempdir().unwrap();
    let payments = project.path().join("services/payments");
    fs::create_dir_all(&payments).unwrap();
    fs::create_dir_all(project.path().join(".claude/rules")).unwrap();
    fs::write(project.path().join("CLAUDE.md"), "# Claude root\n").unwrap();
    fs::write(project.path().join("AGENTS.md"), "# Root rules\n").unwrap();
    fs::write(payments.join("AGENTS.md"), "# Ignored local base\n").unwrap();
    fs::write(payments.join("AGENTS.override.md"), "# Payments override\n").unwrap();
    fs::write(
        project.path().join(".claude/rules/api.md"),
        "---\npaths:\n  - \"services/payments/**/*.rs\"\n---\n# API rules\n",
    )
    .unwrap();

    let report = doctor_without_writes_from(project.path(), &payments, home.path());

    let codex = report["chains"]
        .as_array()
        .unwrap()
        .iter()
        .find(|chain| chain["harness"] == "codex")
        .unwrap();
    let effective: Vec<_> = codex["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["effective"] == true)
        .collect();
    assert_eq!(effective[0]["source"], "AGENTS.md");
    assert_eq!(effective[0]["order"], 1);
    assert_eq!(
        effective[1]["source"],
        "services/payments/AGENTS.override.md"
    );
    assert_eq!(effective[1]["order"], 2);
    assert_eq!(effective[1]["classification"], "override");
    let ignored = codex["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["source"] == "services/payments/AGENTS.md")
        .unwrap();
    assert_eq!(ignored["effective"], false);
    assert!(
        ignored["reason"]
            .as_str()
            .unwrap()
            .contains("AGENTS.override.md")
    );

    let claude = report["chains"]
        .as_array()
        .unwrap()
        .iter()
        .find(|chain| chain["harness"] == "claude_code")
        .unwrap();
    let path_rule = claude["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["source"] == ".claude/rules/api.md")
        .unwrap();
    assert_eq!(path_rule["classification"], "path_scoped");
    assert_eq!(path_rule["load_mode"], "path_scoped");
    assert_eq!(path_rule["path_patterns"][0], "services/payments/**/*.rs");
}

#[test]
fn marker_health_and_routing_drift_are_reported_without_repair() {
    let project = init_repo();
    let home = tempfile::tempdir().unwrap();
    fs::create_dir_all(project.path().join(".claude/rules")).unwrap();
    fs::write(
        project.path().join("CLAUDE.md"),
        format!("{MARKER_START}\nstale routing\n{MARKER_END}\n"),
    )
    .unwrap();
    fs::write(
        project.path().join(".claude/rules/current.md"),
        full_block(),
    )
    .unwrap();
    for (directory, content) in [
        ("incomplete", MARKER_START.to_string()),
        (
            "duplicate",
            format!("{MARKER_START}\na\n{MARKER_END}\n{MARKER_START}\nb\n{MARKER_END}\n"),
        ),
        (
            "nested",
            format!("{MARKER_START}\n{MARKER_START}\n{MARKER_END}\n{MARKER_END}\n"),
        ),
        ("crossed", format!("{MARKER_END}\n{MARKER_START}\n")),
    ] {
        let path = project.path().join(directory);
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("CLAUDE.md"), content).unwrap();
    }

    let report = doctor_without_writes(project.path(), home.path());
    let source = |path: &str| {
        report["sources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|source| source["path"] == path)
            .unwrap()
    };

    assert_eq!(
        source("CLAUDE.md")["routing_asset_drift"]["status"],
        "drifted"
    );
    assert_eq!(
        source(".claude/rules/current.md")["routing_asset_drift"]["status"],
        "current"
    );
    for (path, issue) in [
        ("incomplete/CLAUDE.md", "incomplete"),
        ("duplicate/CLAUDE.md", "duplicate"),
        ("nested/CLAUDE.md", "nested"),
        ("crossed/CLAUDE.md", "crossed"),
    ] {
        assert_eq!(source(path)["marker_health"]["status"], "invalid");
        assert!(
            source(path)["marker_health"]["issues"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == issue)
        );
    }
    assert!(
        report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| { finding["code"] == "routing_asset_drift" })
    );
    assert!(report["placement_findings"].as_array().unwrap().is_empty());
}

#[test]
fn thin_pointer_is_an_adapter_but_not_claimed_as_an_official_import() {
    let project = init_repo();
    let home = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("CLAUDE.md"),
        "# Claude adapter\n\nRead AGENTS.md as the canonical project instructions.\n",
    )
    .unwrap();
    fs::write(project.path().join("AGENTS.md"), "# Shared rules\n").unwrap();

    let report = doctor_without_writes(project.path(), home.path());

    assert_eq!(report["canonical"]["path"], "AGENTS.md");
    assert_eq!(report["canonical"]["basis"], "thin_pointer");
    let claude = report["chains"]
        .as_array()
        .unwrap()
        .iter()
        .find(|chain| chain["harness"] == "claude_code")
        .unwrap();
    assert_eq!(claude["entries"][0]["classification"], "adapter");
    let referenced = claude["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["source"] == "AGENTS.md")
        .unwrap();
    assert_eq!(referenced["load_mode"], "referenced");
    assert_eq!(referenced["effective"], false);
    assert!(
        referenced["reason"]
            .as_str()
            .unwrap()
            .contains("not Claude Code @path import syntax")
    );
}

#[test]
fn explicit_reverse_pointer_keeps_claude_canonical_and_agents_as_adapter() {
    let project = init_repo();
    let home = tempfile::tempdir().unwrap();
    fs::write(project.path().join("CLAUDE.md"), "# Canonical rules\n").unwrap();
    fs::write(
        project.path().join("AGENTS.md"),
        "# Codex adapter\n\nRead CLAUDE.md for canonical project instructions.\n",
    )
    .unwrap();
    fs::write(
        project.path().join(".engram.toml"),
        "[instructions]\ncanonical = \"CLAUDE.md\"\n",
    )
    .unwrap();

    let report = doctor_without_writes(project.path(), home.path());
    assert_eq!(report["canonical"]["path"], "CLAUDE.md");
    assert_eq!(report["canonical"]["basis"], "explicit_config");
    let codex = report["chains"]
        .as_array()
        .unwrap()
        .iter()
        .find(|chain| chain["harness"] == "codex")
        .unwrap();
    assert_eq!(codex["entries"][0]["source"], "AGENTS.md");
    assert_eq!(codex["entries"][0]["classification"], "adapter");
    let referenced = codex["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["source"] == "CLAUDE.md")
        .unwrap();
    assert_eq!(referenced["load_mode"], "referenced");
    assert_eq!(referenced["effective"], false);
}

#[test]
fn unsupported_harness_is_explicitly_best_effort() {
    let project = init_repo();
    let home = tempfile::tempdir().unwrap();
    fs::write(project.path().join("GEMINI.md"), "# Gemini rules\n").unwrap();

    let report = doctor_without_writes(project.path(), home.path());
    let gemini = report["chains"]
        .as_array()
        .unwrap()
        .iter()
        .find(|chain| chain["harness"] == "gemini_cli")
        .unwrap();
    assert_eq!(gemini["support"], "best_effort");
    assert_eq!(gemini["total_loaded_bytes"], 0);
    assert_eq!(gemini["entries"][0]["load_mode"], "best_effort");
    assert_eq!(gemini["entries"][0]["effective"], false);
    assert!(gemini["entries"][0]["order"].is_null());
    assert!(
        gemini["entries"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("not inferred")
    );
    assert!(
        report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| { finding["code"] == "best_effort_harness" })
    );
}

#[test]
fn codex_fallback_and_byte_limit_follow_codex_configuration() {
    let project = init_repo();
    let home = tempfile::tempdir().unwrap();
    fs::create_dir_all(home.path().join(".codex")).unwrap();
    fs::write(
        home.path().join(".codex/config.toml"),
        "project_doc_fallback_filenames = [\"TEAM_GUIDE.md\"]\nproject_doc_max_bytes = 10\n",
    )
    .unwrap();
    fs::write(
        project.path().join("TEAM_GUIDE.md"),
        "1234567890truncated\n",
    )
    .unwrap();

    let report = doctor_without_writes(project.path(), home.path());
    let codex = report["chains"]
        .as_array()
        .unwrap()
        .iter()
        .find(|chain| chain["harness"] == "codex")
        .unwrap();
    assert_eq!(codex["project_document_max_bytes"], 10);
    let fallback = codex["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["source"] == "TEAM_GUIDE.md")
        .unwrap();
    assert_eq!(fallback["effective"], true);
    assert_eq!(fallback["loaded_bytes"], 10);
    assert_eq!(fallback["truncated"], true);
    assert!(
        report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| { finding["code"] == "codex_project_document_limit_reached" })
    );
}

#[test]
fn codex_fallback_paths_outside_the_repository_fail_closed() {
    let project = init_repo();
    let home = tempfile::tempdir().unwrap();
    fs::create_dir_all(home.path().join(".codex")).unwrap();
    fs::write(
        home.path().join(".codex/config.toml"),
        "project_doc_fallback_filenames = [\"../outside.md\"]\n",
    )
    .unwrap();
    fs::write(home.path().join("outside.md"), "private outside content\n").unwrap();

    let report = doctor_without_writes(project.path(), home.path());

    assert!(
        report["sources"]
            .as_array()
            .unwrap()
            .iter()
            .all(|source| source["path"] != "../outside.md")
    );
    assert!(
        report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| {
                finding["code"] == "invalid_codex_fallback_filename"
                    && finding["message"]
                        .as_str()
                        .unwrap()
                        .contains("../outside.md")
            })
    );
}

#[test]
fn human_report_bypasses_runtime_config_and_llm_state() {
    let project = init_repo();
    let home = tempfile::tempdir().unwrap();
    fs::write(project.path().join("AGENTS.md"), "# Rules\n").unwrap();
    let invalid_config = home.path().join("invalid.toml");
    fs::write(&invalid_config, "this is not = valid toml [").unwrap();
    let project_before = snapshot(project.path());
    let home_before = snapshot(home.path());

    let output = Command::new(bin())
        .args([
            "--config",
            invalid_config.to_str().unwrap(),
            "instructions",
            "doctor",
        ])
        .current_dir(project.path())
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("ENGRAM_DATA_DIR", home.path().join("engram-data"))
        .env("ENGRAM_LLM_PROVIDER", "definitely-not-a-provider")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Instruction doctor (read-only)"));
    assert!(stdout.contains("Canonical source: AGENTS.md"));
    assert!(stdout.contains("lines, 8 bytes, ~2 tokens"));
    assert!(stdout.contains("codex [formal]"));
    assert!(stdout.contains("loaded 8 bytes; project limit 32768 bytes"));
    assert!(stdout.contains("canonical; startup; effective; loaded 8 bytes"));
    assert_eq!(snapshot(project.path()), project_before);
    assert_eq!(snapshot(home.path()), home_before);
    assert!(!home.path().join("engram-data").exists());
}

#[test]
fn long_claude_root_reports_context_budget_warning_without_deletion_advice() {
    let project = init_repo();
    let home = tempfile::tempdir().unwrap();
    let content = (0..201)
        .map(|index| format!("- Project invariant {index}\n"))
        .collect::<String>();
    fs::write(project.path().join("CLAUDE.md"), content).unwrap();

    let report = doctor_without_writes(project.path(), home.path());
    let finding = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|finding| finding["code"] == "claude_root_over_200_lines")
        .unwrap();
    assert_eq!(finding["severity"], "warning");
    assert!(finding["message"].as_str().unwrap().contains("201 lines"));
    assert!(!finding["message"].as_str().unwrap().contains("delete"));

    let placement = report["placement_findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|finding| finding["code"] == "context_budget_pressure")
        .unwrap();
    assert_eq!(placement["action"], "review");
    assert_eq!(placement["destination"], "no_change");
    assert_eq!(placement["protected"], false);
    assert!(
        placement["rationale"]
            .as_str()
            .unwrap()
            .contains("never deletion evidence by itself")
    );
}

#[test]
fn placement_diagnostics_remove_generic_text_and_protect_local_knowledge() {
    let project = init_repo();
    let home = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("AGENTS.md"),
        "# Project instructions\n\n\
Think step by step and be helpful.\n\n\
- Rust filesystem errors in this repository must use `anyhow::Context`.\n\
- Production deployment uses the private `opsctl release` workflow.\n\
- The internal `artifact-index` tool is the only supported index writer.\n\
- Database migrations must remain backward-compatible for one release.\n\
- Enterprise tenants must never receive consumer trial entitlements.\n\
- Authentication checks must never be bypassed.\n",
    )
    .unwrap();

    let report = doctor_without_writes(project.path(), home.path());
    let findings = report["placement_findings"].as_array().unwrap();

    let generic = findings
        .iter()
        .find(|finding| finding["category"] == "generic_harness")
        .unwrap();
    assert_eq!(generic["action"], "remove");
    assert_eq!(generic["destination"], "no_change");
    assert_eq!(generic["protected"], false);
    assert!(
        generic["evidence"]
            .as_str()
            .unwrap()
            .contains("step by step")
    );

    for category in [
        "team_convention",
        "private_deployment",
        "internal_tool",
        "database_migration",
        "business_boundary",
        "security_requirement",
    ] {
        let finding = findings
            .iter()
            .find(|finding| finding["category"] == category)
            .unwrap_or_else(|| panic!("missing protected category {category}"));
        assert_eq!(finding["protected"], true, "category {category}");
        assert_ne!(finding["action"], "remove", "category {category}");
        assert!(
            finding["rationale"]
                .as_str()
                .unwrap()
                .contains("cannot be inferred"),
            "category {category}"
        );
    }

    let deployment = findings
        .iter()
        .find(|finding| finding["category"] == "private_deployment")
        .unwrap();
    assert_eq!(deployment["action"], "move");
    assert_eq!(deployment["destination"], "agent_skill");

    let security = findings
        .iter()
        .find(|finding| finding["category"] == "security_requirement")
        .unwrap();
    assert_eq!(security["action"], "reinforce");
    assert_eq!(security["destination"], "enforcement");
}

#[test]
fn wrong_layer_content_routes_to_path_rules_skills_and_wiki() {
    let project = init_repo();
    let home = tempfile::tempdir().unwrap();
    fs::create_dir_all(project.path().join("crates/store")).unwrap();
    fs::write(
        project.path().join("AGENTS.md"),
        "# Project instructions\n\n\
When working under `crates/store/`, keep SQLite writes behind the writer actor.\n\n\
## Release procedure\n\n\
- Run the release build.\n\
\n\
- Verify the signed artifact.\n\
\n\
- Publish the release notes.\n\n\
## Background\n\n\
We chose SQLite because the repository is designed for a single local writer; rejected alternatives and benchmark evidence belong with this history.\n",
    )
    .unwrap();

    let report = doctor_without_writes(project.path(), home.path());
    let findings = report["placement_findings"].as_array().unwrap();

    let scoped = findings
        .iter()
        .find(|finding| finding["category"] == "component_scope")
        .unwrap();
    assert_eq!(scoped["action"], "move");
    assert_eq!(scoped["destination"], "path_rules");

    let workflow = findings
        .iter()
        .find(|finding| finding["category"] == "workflow")
        .unwrap();
    assert_eq!(workflow["action"], "move");
    assert_eq!(workflow["destination"], "agent_skill");

    let history = findings
        .iter()
        .find(|finding| finding["category"] == "history_and_evidence")
        .unwrap();
    assert_eq!(history["action"], "move");
    assert_eq!(history["destination"], "wiki");
    assert!(
        findings
            .iter()
            .all(|finding| finding["code"] != "stale_path_reference")
    );
}

#[test]
fn claude_rule_frontmatter_distinguishes_root_and_path_destinations() {
    let project = init_repo();
    let home = tempfile::tempdir().unwrap();
    fs::create_dir_all(project.path().join(".claude/rules")).unwrap();
    fs::write(
        project.path().join(".claude/rules/global.md"),
        "Database migrations must remain reversible.\n",
    )
    .unwrap();
    fs::write(
        project.path().join(".claude/rules/store.md"),
        "---\npaths:\n  - crates/store/**\n---\nDatabase migrations must remain backward-compatible.\n",
    )
    .unwrap();

    let report = doctor_without_writes(project.path(), home.path());
    let findings = report["placement_findings"].as_array().unwrap();
    let global = findings
        .iter()
        .find(|finding| finding["source"] == ".claude/rules/global.md")
        .unwrap();
    assert_eq!(global["destination"], "root_instructions");
    let scoped = findings
        .iter()
        .find(|finding| finding["source"] == ".claude/rules/store.md")
        .unwrap();
    assert_eq!(scoped["destination"], "path_rules");
}

#[test]
fn duplicate_conflict_stale_reference_and_missing_skill_are_diagnosed() {
    let project = init_repo();
    let home = tempfile::tempdir().unwrap();
    fs::create_dir_all(project.path().join("scripts")).unwrap();
    fs::create_dir_all(project.path().join("docs")).unwrap();
    fs::write(project.path().join("scripts/current.sh"), "#!/bin/sh\n").unwrap();
    fs::write(project.path().join("docs/current.md"), "# Current\n").unwrap();
    fs::create_dir_all(project.path().join(".agents/skills/existing")).unwrap();
    fs::write(
        project.path().join(".agents/skills/existing/SKILL.md"),
        "---\nname: existing\n---\n",
    )
    .unwrap();
    fs::write(
        project.path().join("AGENTS.md"),
        "# Project instructions\n\n\
- Always run cargo test before merging.\n\
- Never run cargo test before merging.\n\
- See `docs/removed.md` for the release policy.\n\
- Run `./scripts/retired.sh --prod`.\n\
- Run `./scripts/current.sh`.\n\
- See [the current guide](docs/current.md#usage).\n\
- Use the `missing-release` Skill.\n\
- Use the `existing` Skill.\n\
- Keep audit logs for thirty days.\n\
- **Keep audit logs** for thirty days!\n\
- Keep audit logs for thirty days.\n",
    )
    .unwrap();

    let report = doctor_without_writes(project.path(), home.path());
    let findings = report["placement_findings"].as_array().unwrap();

    for code in [
        "contradictory_guidance",
        "duplicate_guidance",
        "stale_path_reference",
        "stale_command_reference",
        "missing_referenced_skill",
    ] {
        assert!(
            findings.iter().any(|finding| finding["code"] == code),
            "missing placement finding {code}"
        );
    }

    let duplicates: Vec<_> = findings
        .iter()
        .filter(|finding| finding["code"] == "duplicate_guidance")
        .collect();
    assert_eq!(duplicates.len(), 2);
    assert!(duplicates.iter().any(|finding| {
        finding["evidence"]
            .as_str()
            .unwrap()
            .starts_with("Normalized")
    }));
    assert!(
        duplicates
            .iter()
            .any(|finding| finding["evidence"].as_str().unwrap().starts_with("Exact"))
    );
    for duplicate in duplicates {
        assert_eq!(duplicate["action"], "remove");
        assert_eq!(duplicate["destination"], "no_change");
        assert!(duplicate["related_sources"].as_array().unwrap().len() == 1);
    }

    assert!(findings.iter().all(|finding| {
        let evidence = finding["evidence"].as_str().unwrap();
        !evidence.contains("scripts/current.sh")
            && !evidence.contains("docs/current.md")
            && !evidence.contains("`existing` Skill")
    }));
}

#[test]
fn claude_imports_are_explicitly_counted_as_loaded_context() {
    let project = init_repo();
    let home = tempfile::tempdir().unwrap();
    fs::write(project.path().join("CLAUDE.md"), "@AGENTS.md\n").unwrap();
    fs::write(
        project.path().join("AGENTS.md"),
        "# Shared rules\n\nUse the repository formatter.\n",
    )
    .unwrap();

    let report = doctor_without_writes(project.path(), home.path());
    let imported = report["placement_findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|finding| finding["code"] == "imported_context_counts")
        .unwrap();
    assert_eq!(imported["action"], "review");
    assert_eq!(imported["destination"], "no_change");
    assert!(
        imported["rationale"]
            .as_str()
            .unwrap()
            .contains("not a token saving")
    );
}

#[test]
fn human_report_explains_placement_action_destination_and_evidence() {
    let project = init_repo();
    let home = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("AGENTS.md"),
        "# Rules\n\nThink step by step and be helpful.\n",
    )
    .unwrap();
    let project_before = snapshot(project.path());
    let home_before = snapshot(home.path());

    let output = Command::new(bin())
        .args(["instructions", "doctor"])
        .current_dir(project.path())
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("ENGRAM_DATA_DIR", home.path().join("engram-data"))
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Placement diagnostics"));
    assert!(stdout.contains("generic_harness_guidance"));
    assert!(stdout.contains("action remove; destination no_change; protected no"));
    assert!(stdout.contains("Evidence:"));
    assert_eq!(snapshot(project.path()), project_before);
    assert_eq!(snapshot(home.path()), home_before);
}

#[test]
fn semantic_placement_output_is_deterministic_and_read_only() {
    let project = init_repo();
    let home = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("AGENTS.md"),
        "# Rules\n\nThink step by step.\n\nDatabase migrations must remain reversible.\n",
    )
    .unwrap();
    let project_before = snapshot(project.path());
    let home_before = snapshot(home.path());

    let first = assert_success(run_doctor(project.path(), home.path(), &[]));
    let second = assert_success(run_doctor(project.path(), home.path(), &[]));

    assert_eq!(first, second);
    assert_eq!(snapshot(project.path()), project_before);
    assert_eq!(snapshot(home.path()), home_before);
    assert!(!home.path().join("engram-data").exists());
}

#[cfg(unix)]
#[test]
fn external_instruction_symlink_fails_closed_and_is_never_read() {
    use std::os::unix::fs::symlink;

    let project = init_repo();
    let home = tempfile::tempdir().unwrap();
    let outside = home.path().join("outside.md");
    fs::write(&outside, "outside repository content\n").unwrap();
    symlink(&outside, project.path().join("CLAUDE.md")).unwrap();

    let report = doctor_without_writes(project.path(), home.path());

    assert!(report["canonical"]["path"].is_null());
    let source = report["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|source| source["path"] == "CLAUDE.md")
        .unwrap();
    assert_eq!(source["symlink"]["safe"], false);
    assert_eq!(source["byte_count"], 0);
    assert!(source["read_error"].as_str().unwrap().contains("not read"));
    assert!(
        report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| { finding["code"] == "unsafe_instruction_symlink" })
    );
}
