//! Integration tests for engram routing instructions.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use engram_core::routing_skills::MANAGED_MARKER;
use engram_core::{MARKER_END, MARKER_START};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_engram")
}

fn run_engram(project: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .current_dir(project)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("ENGRAM_DATA_DIR", home.join(".engram-data"))
        .output()
        .unwrap()
}

fn assert_success(output: Output) -> String {
    assert!(
        output.status.success(),
        "command failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn assert_failure(output: Output) -> String {
    assert!(
        !output.status.success(),
        "command unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stderr).unwrap()
}

#[test]
fn skill_conflict_preflight_keeps_instruction_file_unchanged_until_force() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let target = project.path().join("CLAUDE.md");
    let original = b"# Project\n\nKeep this exact.\n".to_vec();
    fs::write(&target, &original).unwrap();

    let unmanaged = project
        .path()
        .join(".claude/skills/engram-retrieval/SKILL.md");
    let unmanaged_content = b"---\nname: engram-retrieval\n---\nuser skill\n".to_vec();
    fs::create_dir_all(unmanaged.parent().unwrap()).unwrap();
    fs::write(&unmanaged, &unmanaged_content).unwrap();

    let output = run_engram(project.path(), home.path(), &["install-instructions"]);
    let stderr = assert_failure(output);
    assert!(stderr.contains("refusing to overwrite unmanaged skill"));
    assert_eq!(fs::read(&target).unwrap(), original);
    assert_eq!(fs::read(&unmanaged).unwrap(), unmanaged_content);
    assert!(
        !project
            .path()
            .join(".claude/skills/engram-handoff/SKILL.md")
            .exists()
    );

    let output = run_engram(
        project.path(),
        home.path(),
        &["install-instructions", "--skills-force"],
    );
    assert_success(output);

    let updated = fs::read_to_string(&target).unwrap();
    assert!(updated.contains(MARKER_START));
    assert!(updated.contains("# Project"));
    assert!(
        fs::read_to_string(&unmanaged)
            .unwrap()
            .contains(MANAGED_MARKER)
    );
    assert!(
        project
            .path()
            .join(".claude/skills/engram-handoff/SKILL.md")
            .exists()
    );
}

#[test]
fn no_skills_writes_only_instruction_snippet() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    let output = run_engram(
        project.path(),
        home.path(),
        &["install-instructions", "--no-skills"],
    );
    assert_success(output);

    let claude_md = fs::read_to_string(project.path().join("CLAUDE.md")).unwrap();
    assert!(claude_md.contains(MARKER_START));
    assert!(claude_md.contains("Use the installed engram Agent Skills"));
    assert!(!project.path().join(".claude/skills").exists());
    assert!(!project.path().join(".agents/skills").exists());
}

#[test]
fn install_instructions_updates_only_markered_block_and_backs_up_original() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let target = project.path().join("CLAUDE.md");
    let original = format!(
        "# Project\n\nKeep this intro.\n\n{MARKER_START}\nold engram block\n{MARKER_END}\n\nKeep this tail.\n"
    );
    fs::write(&target, &original).unwrap();

    let output = run_engram(
        project.path(),
        home.path(),
        &["install-instructions", "--no-skills"],
    );
    assert_success(output);

    let updated = fs::read_to_string(&target).unwrap();
    assert!(updated.contains("Keep this intro."));
    assert!(updated.contains("Keep this tail."));
    assert!(updated.contains(MARKER_START));
    assert!(updated.contains("Use the installed engram Agent Skills"));
    assert!(!updated.contains("old engram block"));

    let backups: Vec<_> = fs::read_dir(project.path())
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("CLAUDE.md.bak-"))
        })
        .collect();
    assert_eq!(
        backups.len(),
        1,
        "install-instructions must back up updates"
    );
    assert_eq!(fs::read_to_string(&backups[0]).unwrap(), original);
}

#[test]
fn print_shows_only_snippet_without_mutating() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    let output = run_engram(
        project.path(),
        home.path(),
        &["install-instructions", "--print"],
    );
    let stdout = assert_success(output);

    assert!(stdout.contains("# Would write into:"));
    assert!(stdout.contains(MARKER_START));
    assert!(stdout.contains("Use the installed engram Agent Skills"));
    assert!(!stdout.contains("# Skill root:"));
    assert!(!stdout.contains("engram-retrieval/SKILL.md"));
    assert!(!stdout.contains(MANAGED_MARKER));
    assert!(!project.path().join("CLAUDE.md").exists());
    assert!(!project.path().join(".claude/skills").exists());
    assert!(!project.path().join(".agents/skills").exists());
}

#[test]
fn no_skills_print_omits_skill_plan_and_does_not_mutate() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    let output = run_engram(
        project.path(),
        home.path(),
        &["install-instructions", "--print", "--no-skills"],
    );
    let stdout = assert_success(output);

    assert!(stdout.contains(MARKER_START));
    assert!(!stdout.contains("# Skill root:"));
    assert!(!stdout.contains("engram-retrieval/SKILL.md"));
    assert!(!project.path().join("CLAUDE.md").exists());
    assert!(!project.path().join(".claude/skills").exists());
}

#[test]
fn inferred_instruction_targets_select_matching_skill_agents() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    fs::write(project.path().join("AGENTS.md"), "# Agents\n").unwrap();

    let output = run_engram(project.path(), home.path(), &["install-instructions"]);
    assert_success(output);

    assert!(
        project
            .path()
            .join(".agents/skills/engram-retrieval/SKILL.md")
            .exists()
    );
    assert!(!project.path().join(".claude/skills").exists());

    let both_project = tempfile::tempdir().unwrap();
    let both_home = tempfile::tempdir().unwrap();
    fs::write(both_project.path().join("CLAUDE.md"), "# Claude\n").unwrap();
    fs::write(both_project.path().join("AGENTS.md"), "# Agents\n").unwrap();

    let output = run_engram(
        both_project.path(),
        both_home.path(),
        &["install-instructions"],
    );
    assert_success(output);

    assert!(
        both_project
            .path()
            .join(".claude/skills/engram-retrieval/SKILL.md")
            .exists()
    );
    assert!(
        both_project
            .path()
            .join(".agents/skills/engram-retrieval/SKILL.md")
            .exists()
    );
}

#[test]
fn explicit_skill_scope_and_agent_override_instruction_target_inference() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let global_skills = home.path().join("global-skills");
    let global_skills_arg = global_skills.to_string_lossy().to_string();

    let output = run_engram(
        project.path(),
        home.path(),
        &[
            "install-instructions",
            "--target",
            "AGENTS.md",
            "--skills-scope",
            "global",
            "--skills-agent",
            "claude-code",
            "--skills-target-dir",
            &global_skills_arg,
        ],
    );
    assert_success(output);

    assert!(global_skills.join("engram-retrieval/SKILL.md").exists());
    assert!(!home.path().join(".agents/skills").exists());
    assert!(!project.path().join(".claude/skills").exists());
    assert!(!project.path().join(".agents/skills").exists());
}

#[test]
fn explicit_skill_overrides_win_over_instruction_target_inference() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let custom_root = project.path().join("custom-skills");
    let unmanaged = custom_root.join("engram-retrieval/SKILL.md");
    fs::create_dir_all(unmanaged.parent().unwrap()).unwrap();
    fs::write(&unmanaged, "---\nname: engram-retrieval\n---\nuser skill\n").unwrap();

    let custom_root_arg = custom_root.to_str().unwrap();
    let output = run_engram(
        project.path(),
        home.path(),
        &[
            "install-instructions",
            "--target",
            "AGENTS.md",
            "--skills-scope",
            "global",
            "--skills-agent",
            "both",
            "--skills-target-dir",
            custom_root_arg,
            "--skills-force",
        ],
    );
    assert_success(output);

    let overwritten = fs::read_to_string(&unmanaged).unwrap();
    assert!(overwritten.contains(MANAGED_MARKER));
    assert!(custom_root.join("engram-handoff/SKILL.md").exists());
    assert!(!project.path().join(".agents/skills").exists());
    assert!(!project.path().join(".claude/skills").exists());
    assert!(!home.path().join(".agents/skills").exists());
    assert!(!home.path().join(".claude/skills").exists());
}

#[test]
fn legacy_long_marker_block_rerun_upgrades_in_place_to_slim_snippet() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let original = format!(
        "# Project\n\n{MARKER_START}\n## Long-term memory (engram)\n\n### When to reach for each tool\n\n|User says / situation|Tool|\n|---|---|\n|search memory|`memory_query`|\n{MARKER_END}\n\nKeep me.\n"
    );
    fs::write(project.path().join("CLAUDE.md"), original).unwrap();

    let output = run_engram(
        project.path(),
        home.path(),
        &["install-instructions", "--no-skills"],
    );
    assert_success(output);

    let updated = fs::read_to_string(project.path().join("CLAUDE.md")).unwrap();
    assert!(updated.contains("# Project"));
    assert!(updated.contains("Keep me."));
    assert!(updated.contains("Use the installed engram Agent Skills"));
    assert!(!updated.contains("When to reach for each tool"));
    assert!(!updated.contains("|User says / situation|Tool|"));
    assert_eq!(
        updated.lines().filter(|line| *line == MARKER_START).count(),
        1
    );
    assert_eq!(
        updated.lines().filter(|line| *line == MARKER_END).count(),
        1
    );
}
