//! Black-box acceptance tests for instruction proposal and human-review stewardship.

use std::collections::BTreeMap;
use std::fs;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_engram")
}

fn reserve_addr() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    addr.to_string()
}

struct Server {
    child: Child,
    url: String,
}

impl Server {
    fn start(data_dir: &Path, project: &str, addr: &str) -> Self {
        let mut child = Command::new(bin())
            .args([
                "--data-dir",
                data_dir.to_str().unwrap(),
                "serve",
                "--transport",
                "http",
                "--bind",
                addr,
                "--no-watcher",
                "--workspace",
                "default",
                "--project",
                project,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if TcpStream::connect(addr).is_ok() {
                return Self {
                    child,
                    url: format!("http://{addr}"),
                };
            }
            thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
        panic!("engram server did not become ready at {addr}");
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn run(project: &Path, data_dir: &Path, server_url: &str, args: &[&str]) -> Output {
    Command::new(bin())
        .args(["--data-dir", data_dir.to_str().unwrap()])
        .args(args)
        .current_dir(project)
        .env("ENGRAM_SERVER_URL", server_url)
        .output()
        .unwrap()
}

fn json_success(output: Output) -> Value {
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
            let path = entry.path();
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            let metadata = fs::symlink_metadata(&path).unwrap();
            if metadata.is_dir() {
                out.insert(relative.clone(), b"directory".to_vec());
                visit(root, &path, out);
            } else {
                out.insert(relative, fs::read(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    visit(root, root, &mut out);
    out
}

#[test]
fn durable_rule_stages_reviewable_proposal_without_touching_repository_or_wiki_target() {
    let repository = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(repository.path())
        .status()
        .unwrap();
    fs::write(
        repository.path().join("AGENTS.md"),
        "# Rules\n\nNever Keep SQLite writes behind the single writer actor.\n",
    )
    .unwrap();
    let project = "instruction-proposals";
    let addr = reserve_addr();
    let server = Server::start(data.path(), project, &addr);

    let wrote = run(
        repository.path(),
        data.path(),
        &server.url,
        &[
            "write-page",
            "--workspace",
            "default",
            "--project",
            project,
            "--path",
            "_rules/single-writer.md",
            "--kind",
            "rule",
            "--body",
            "# Single writer\n\nKeep SQLite writes behind the single writer actor.",
        ],
    );
    assert!(
        wrote.status.success(),
        "{}",
        String::from_utf8_lossy(&wrote.stderr)
    );

    let repository_before = snapshot(repository.path());
    let wiki_before = snapshot(&data.path().join("wiki"));
    let proposal = json_success(run(
        repository.path(),
        data.path(),
        &server.url,
        &[
            "instructions",
            "propose",
            "--workspace",
            "default",
            "--project",
            project,
            "--rule",
            "_rules/single-writer.md",
            "--target",
            "AGENTS.md",
            "--json",
        ],
    ));
    let proposal_id = proposal["proposal_id"].as_str().unwrap();
    assert_eq!(proposal["target_kind"], "project_instruction");
    assert_eq!(proposal["operation"], "add");
    assert_eq!(snapshot(repository.path()), repository_before);
    assert_eq!(snapshot(&data.path().join("wiki")), wiki_before);

    let listed = json_success(run(
        repository.path(),
        data.path(),
        &server.url,
        &[
            "pending-writes",
            "list",
            "--workspace",
            "default",
            "--project",
            project,
            "--json",
        ],
    ));
    assert!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .any(|row| { row["id"] == proposal_id && row["target_kind"] == "project_instruction" })
    );

    let shown = json_success(run(
        repository.path(),
        data.path(),
        &server.url,
        &[
            "pending-writes",
            "show",
            proposal_id,
            "--workspace",
            "default",
            "--project",
            project,
            "--json",
        ],
    ));
    assert_eq!(
        shown["summary"]["target_context_layer"],
        "root_instructions"
    );
    assert!(
        shown["proposed_content"]
            .as_str()
            .unwrap()
            .contains("single writer actor")
    );

    let diff = json_success(run(
        repository.path(),
        data.path(),
        &server.url,
        &[
            "pending-writes",
            "diff",
            proposal_id,
            "--workspace",
            "default",
            "--project",
            project,
            "--json",
        ],
    ));
    assert!(
        diff["diff"]
            .as_str()
            .unwrap()
            .contains("+Keep SQLite writes")
    );

    let rejected = run(
        repository.path(),
        data.path(),
        &server.url,
        &[
            "pending-writes",
            "reject",
            proposal_id,
            "--workspace",
            "default",
            "--project",
            project,
            "--reason",
            "wording needs work",
            "--json",
        ],
    );
    assert!(
        rejected.status.success(),
        "{}",
        String::from_utf8_lossy(&rejected.stderr)
    );
    assert_eq!(snapshot(repository.path()), repository_before);
    assert_eq!(snapshot(&data.path().join("wiki")), wiki_before);

    let rule = json_success(run(
        repository.path(),
        data.path(),
        &server.url,
        &[
            "read-page",
            "--workspace",
            "default",
            "--project",
            project,
            "--path",
            "_rules/single-writer.md",
            "--json",
        ],
    ));
    assert_eq!(
        rule["body"],
        "# Single writer\n\nKeep SQLite writes behind the single writer actor."
    );
}

#[test]
fn selected_doctor_finding_stages_stale_deletion_and_survives_server_restart() {
    let repository = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(repository.path())
        .status()
        .unwrap();
    fs::write(
        repository.path().join("AGENTS.md"),
        "# Rules\n\nThink step by step and be helpful.\n\nKeep project-specific invariants.\n",
    )
    .unwrap();
    let project = "instruction-findings";
    let addr = reserve_addr();
    let server = Server::start(data.path(), project, &addr);
    let repository_before = snapshot(repository.path());

    let proposal = json_success(run(
        repository.path(),
        data.path(),
        &server.url,
        &[
            "instructions",
            "propose",
            "--workspace",
            "default",
            "--project",
            project,
            "--finding",
            "generic_harness_guidance",
            "--source",
            "AGENTS.md",
            "--line",
            "3",
            "--json",
        ],
    ));
    let proposal_id = proposal["proposal_id"].as_str().unwrap().to_owned();
    assert_eq!(proposal["operation"], "stale_delete");
    assert_eq!(snapshot(repository.path()), repository_before);
    drop(server);

    let restarted = Server::start(data.path(), project, &addr);
    let shown = json_success(run(
        repository.path(),
        data.path(),
        &restarted.url,
        &[
            "pending-writes",
            "show",
            &proposal_id,
            "--workspace",
            "default",
            "--project",
            project,
            "--json",
        ],
    ));
    assert_eq!(shown["summary"]["operation"], "stale_delete");
    assert!(
        !shown["proposed_content"]
            .as_str()
            .unwrap()
            .contains("Think step by step")
    );
    assert_eq!(shown["provenance"][0]["kind"], "doctor_finding");
    assert_eq!(snapshot(repository.path()), repository_before);
}

#[test]
fn reviewer_edits_and_approves_instruction_without_applying_target_changes() {
    let repository = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(repository.path())
        .status()
        .unwrap();
    fs::write(repository.path().join("AGENTS.md"), "# Rules\n").unwrap();
    let project = "instruction-review";
    let addr = reserve_addr();
    let server = Server::start(data.path(), project, &addr);

    let wrote = run(
        repository.path(),
        data.path(),
        &server.url,
        &[
            "write-page",
            "--workspace",
            "default",
            "--project",
            project,
            "--path",
            "_rules/single-writer.md",
            "--kind",
            "rule",
            "--body",
            "# Single writer\n\nKeep SQLite writes behind the single writer actor.",
        ],
    );
    assert!(wrote.status.success());

    let repository_before = snapshot(repository.path());
    let wiki_before = snapshot(&data.path().join("wiki"));
    let proposal = json_success(run(
        repository.path(),
        data.path(),
        &server.url,
        &[
            "instructions",
            "propose",
            "--workspace",
            "default",
            "--project",
            project,
            "--rule",
            "_rules/single-writer.md",
            "--target",
            "AGENTS.md",
            "--json",
        ],
    ));
    let proposal_id = proposal["proposal_id"].as_str().unwrap().to_owned();
    let original = json_success(run(
        repository.path(),
        data.path(),
        &server.url,
        &[
            "pending-writes",
            "show",
            &proposal_id,
            "--workspace",
            "default",
            "--project",
            project,
            "--json",
        ],
    ));
    let original_content = original["proposed_content"].as_str().unwrap().to_owned();
    let original_approval_sha = original["approval_sha256"].as_str().unwrap().to_owned();
    let reviewed_content = "# Rules\n\nKeep every SQLite write behind the single writer actor.\n";

    let edited = json_success(run(
        repository.path(),
        data.path(),
        &server.url,
        &[
            "pending-writes",
            "edit",
            &proposal_id,
            "--workspace",
            "default",
            "--project",
            project,
            "--content",
            reviewed_content,
            "--json",
        ],
    ));
    assert_eq!(edited["status"], "pending");
    assert_eq!(edited["review_revision"], 1);
    assert_ne!(edited["approval_sha256"], original_approval_sha);
    assert_eq!(snapshot(repository.path()), repository_before);
    assert_eq!(snapshot(&data.path().join("wiki")), wiki_before);

    drop(server);
    let restarted = Server::start(data.path(), project, &addr);
    let reviewed = json_success(run(
        repository.path(),
        data.path(),
        &restarted.url,
        &[
            "pending-writes",
            "show",
            &proposal_id,
            "--workspace",
            "default",
            "--project",
            project,
            "--json",
        ],
    ));
    assert_eq!(reviewed["proposed_content"], reviewed_content);
    assert!(
        reviewed["unified_diff"]
            .as_str()
            .unwrap()
            .contains("+Keep every SQLite write")
    );
    let estimate = |body: &str| i64::try_from(body.len().div_ceil(4)).unwrap();
    assert_eq!(
        reviewed["estimated_token_delta"],
        estimate(reviewed_content) - estimate("# Rules\n")
    );
    assert_eq!(reviewed["review_revision"], 1);
    assert_eq!(reviewed["revisions"].as_array().unwrap().len(), 2);
    assert_eq!(
        reviewed["revisions"][0]["proposed_content"],
        original_content
    );
    assert_eq!(
        reviewed["revisions"][1]["proposed_content"],
        reviewed_content
    );
    let reviewed_diff = run(
        repository.path(),
        data.path(),
        &restarted.url,
        &[
            "pending-writes",
            "diff",
            &proposal_id,
            "--workspace",
            "default",
            "--project",
            project,
        ],
    );
    assert!(
        reviewed_diff.status.success(),
        "diff failed: {}",
        String::from_utf8_lossy(&reviewed_diff.stderr)
    );
    assert!(
        String::from_utf8_lossy(&reviewed_diff.stdout)
            .contains("+Keep every SQLite write behind the single writer actor.")
    );

    let approved = json_success(run(
        repository.path(),
        data.path(),
        &restarted.url,
        &[
            "pending-writes",
            "approve",
            &proposal_id,
            "--workspace",
            "default",
            "--project",
            project,
            "--json",
        ],
    ));
    assert_eq!(approved["status"], "approved");
    assert_eq!(approved["apply_ready"], true);
    assert_eq!(snapshot(repository.path()), repository_before);
    assert_eq!(snapshot(&data.path().join("wiki")), wiki_before);

    let repeated = json_success(run(
        repository.path(),
        data.path(),
        &restarted.url,
        &[
            "pending-writes",
            "approve",
            &proposal_id,
            "--workspace",
            "default",
            "--project",
            project,
            "--json",
        ],
    ));
    assert_eq!(repeated["status"], "approved");
    assert_eq!(repeated["idempotent"], true);
    let final_detail = json_success(run(
        repository.path(),
        data.path(),
        &restarted.url,
        &[
            "pending-writes",
            "show",
            &proposal_id,
            "--workspace",
            "default",
            "--project",
            project,
            "--json",
        ],
    ));
    let approved_events = final_detail["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["event"] == "approved")
        .count();
    assert_eq!(
        approved_events, 1,
        "idempotent retry must not duplicate audit"
    );
    assert_eq!(snapshot(repository.path()), repository_before);
    assert_eq!(snapshot(&data.path().join("wiki")), wiki_before);
}
