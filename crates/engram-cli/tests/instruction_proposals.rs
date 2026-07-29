//! Black-box acceptance tests for instruction proposal and human-review stewardship.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use engram_core::{MARKER_END, MARKER_START};
use serde_json::Value;
use sha2::{Digest, Sha256};

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
        Self::start_with_provider(data_dir, project, addr, None)
    }

    fn start_with_provider(
        data_dir: &Path,
        project: &str,
        addr: &str,
        provider_url: Option<&str>,
    ) -> Self {
        let mut command = Command::new(bin());
        command
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
            .stderr(Stdio::null());
        if let Some(provider_url) = provider_url {
            command
                .env("ENGRAM_LLM_PROVIDER", "openai-compat")
                .env("ENGRAM_LLM_MODEL", "fake-semantic-model")
                .env("ENGRAM_LLM_BASE_URL", provider_url)
                .env("ENGRAM_LLM_COMPAT_STRICT", "true");
        }
        let mut child = command.spawn().unwrap();
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

struct FakeProvider {
    addr: String,
    calls: Arc<AtomicUsize>,
    stopping: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl FakeProvider {
    fn start(structured: Value) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let calls = Arc::new(AtomicUsize::new(0));
        let stopping = Arc::new(AtomicBool::new(false));
        let worker_calls = Arc::clone(&calls);
        let worker_stopping = Arc::clone(&stopping);
        let content = serde_json::to_string(&structured).unwrap();
        let body = serde_json::to_vec(&serde_json::json!({
            "id": "fake-semantic",
            "object": "chat.completion",
            "created": 0,
            "model": "fake-semantic-model",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": content },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 10, "total_tokens": 20 }
        }))
        .unwrap();
        let worker = thread::spawn(move || {
            while !worker_stopping.load(Ordering::SeqCst) {
                let Ok((mut stream, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let mut request = [0_u8; 16 * 1024];
                let _ = stream.read(&mut request);
                worker_calls.fetch_add(1, Ordering::SeqCst);
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(&body);
                let _ = stream.flush();
            }
        });
        Self {
            addr,
            calls,
            stopping,
            worker: Some(worker),
        }
    }

    fn url(&self) -> String {
        format!("http://{}/v1", self.addr)
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Drop for FakeProvider {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(&self.addr);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
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

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
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
fn semantic_rule_assistance_is_bounded_cited_and_manual_only() {
    let repository = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(repository.path())
        .status()
        .unwrap();
    fs::write(
        repository.path().join("AGENTS.md"),
        "# Rules\n\nNever run the full gate before merging.\n",
    )
    .unwrap();
    let provider = FakeProvider::start(serde_json::json!({
        "summary": "The durable rule conflicts with the current root instruction.",
        "findings": [
            {
                "kind": "semantic_conflict",
                "message": "The selected rule requires the full gate while the target forbids it.",
                "citations": [{
                    "source": "_rules/full-gate.md",
                    "quote": "Always run the full gate before merging."
                }]
            },
            {
                "kind": "placement",
                "message": "The rule is universal project policy and belongs in root instructions.",
                "citations": [{
                    "source": "_rules/full-gate.md",
                    "quote": "Always run the full gate before merging."
                }]
            }
        ],
        "proposal": {
            "operation": "update",
            "target_context_layer": "root_instructions",
            "proposed_content": "# Rules\n\nAlways run the full gate before merging.\n",
            "rationale": "Resolve the cited conflict in favor of the explicit durable rule.",
            "citations": [{
                "source": "_rules/full-gate.md",
                "quote": "Always run the full gate before merging."
            }]
        },
        "rejected_candidates": []
    }));
    let project = "instruction-semantic";
    let addr = reserve_addr();
    let provider_url = provider.url();
    let server = Server::start_with_provider(data.path(), project, &addr, Some(&provider_url));
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
            "_rules/full-gate.md",
            "--kind",
            "rule",
            "--body",
            "# Full gate\n\nAlways run the full gate before merging.",
        ],
    );
    assert!(wrote.status.success());

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
            "--rule",
            "_rules/full-gate.md",
            "--target",
            "AGENTS.md",
            "--semantic",
            "--json",
        ],
    ));
    assert_eq!(
        provider.calls(),
        1,
        "semantic assistance has a one-call budget"
    );
    assert_eq!(proposal["status"], "pending");
    assert_eq!(proposal["target_kind"], "project_instruction");
    assert_eq!(proposal["manual_approval_required"], true);
    assert_eq!(proposal["semantic_assistance"]["provider_calls"], 1);
    assert_eq!(proposal["semantic_assistance"]["proposal_count"], 1);
    assert_eq!(proposal["semantic_assistance"]["evidence_count"], 1);
    assert!(
        proposal["semantic_assistance"]["changed_chars"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert_eq!(snapshot(repository.path()), repository_before);

    let shown = json_success(run(
        repository.path(),
        data.path(),
        &server.url,
        &[
            "pending-writes",
            "show",
            proposal["proposal_id"].as_str().unwrap(),
            "--workspace",
            "default",
            "--project",
            project,
            "--json",
        ],
    ));
    assert_eq!(shown["summary"]["status"], "pending");
    assert_eq!(shown["summary"]["target_kind"], "project_instruction");
    assert_eq!(shown["provenance"][0]["kind"], "explicit_user_rule");
    assert_eq!(shown["provenance"][1]["kind"], "semantic_analysis");
    assert_eq!(snapshot(repository.path()), repository_before);
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

#[test]
fn approved_instruction_applies_locally_once_with_cas_backup_and_audit() {
    let repository = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(repository.path())
        .status()
        .unwrap();
    let original = format!(
        "# Rules\n\nKeep this human-owned preface.\n\n{MARKER_START}\nmanaged routing stays byte-identical\n{MARKER_END}\n\nKeep this human-owned tail.\n"
    );
    let target = repository.path().join("AGENTS.md");
    fs::write(&target, &original).unwrap();
    Command::new("git")
        .args(["add", "AGENTS.md"])
        .current_dir(repository.path())
        .status()
        .unwrap();
    let git_index_before = fs::read(repository.path().join(".git/index")).unwrap();

    let project = "instruction-local-apply";
    let addr = reserve_addr();
    let server = Server::start(data.path(), project, &addr);
    let doctor = json_success(run(
        repository.path(),
        data.path(),
        &server.url,
        &["instructions", "doctor", "--json"],
    ));
    assert_eq!(doctor["canonical"]["path"], "AGENTS.md");

    let rule_body = "# Local apply\n\nKeep SQLite writes behind the single writer actor.";
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
            "_rules/local-apply.md",
            "--kind",
            "rule",
            "--body",
            rule_body,
        ],
    );
    assert!(wrote.status.success());
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
            "_rules/local-apply.md",
            "--json",
        ],
    ));
    let proposal_id = proposal["proposal_id"].as_str().unwrap().to_owned();
    let approved = json_success(run(
        repository.path(),
        data.path(),
        &server.url,
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
    assert_eq!(fs::read_to_string(&target).unwrap(), original);

    let expected = format!("{original}\nKeep SQLite writes behind the single writer actor.\n");
    let applied = json_success(run(
        repository.path(),
        data.path(),
        &server.url,
        &[
            "instructions",
            "apply",
            &proposal_id,
            "--workspace",
            "default",
            "--project",
            project,
            "--json",
        ],
    ));
    assert_eq!(applied["proposal_id"], proposal_id);
    assert_eq!(applied["status"], "applied");
    assert_eq!(applied["outcome"], "updated");
    assert_eq!(applied["idempotent"], false);
    assert_eq!(applied["before_sha256"], sha256_hex(original.as_bytes()));
    assert_eq!(applied["after_sha256"], sha256_hex(expected.as_bytes()));
    let backup = PathBuf::from(applied["backup_path"].as_str().unwrap());
    assert_eq!(fs::read_to_string(&backup).unwrap(), original);
    assert_eq!(fs::read_to_string(&target).unwrap(), expected);
    assert_eq!(
        fs::read(repository.path().join(".git/index")).unwrap(),
        git_index_before
    );
    assert_eq!(snapshot(&data.path().join("wiki")), wiki_before);

    let after_first = snapshot(repository.path());
    let repeated = json_success(run(
        repository.path(),
        data.path(),
        &server.url,
        &[
            "instructions",
            "apply",
            &proposal_id,
            "--workspace",
            "default",
            "--project",
            project,
            "--json",
        ],
    ));
    assert_eq!(repeated["status"], "applied");
    assert_eq!(repeated["idempotent"], true);
    assert_eq!(snapshot(repository.path()), after_first);

    let detail = json_success(run(
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
    assert_eq!(detail["summary"]["status"], "approved");
    assert!(detail["summary"]["proposing_actor"].is_object());
    let application = &detail["application"];
    assert!(application["proposing_actor"].is_object());
    assert!(application["approving_actor"].is_object());
    assert!(application["applying_actor"].is_object());
    assert_eq!(
        application["before_sha256"],
        sha256_hex(original.as_bytes())
    );
    assert_eq!(application["after_sha256"], sha256_hex(expected.as_bytes()));
    assert_eq!(application["outcome"], "updated");
    assert_eq!(
        application["backup_path"],
        backup.to_string_lossy().as_ref()
    );
    assert_eq!(
        detail["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["event"] == "applied")
            .count(),
        1,
        "idempotent retry must not duplicate terminal apply audit events"
    );
}

#[test]
fn instruction_apply_rejects_unapproved_and_base_mismatch_without_writing() {
    let repository = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(repository.path())
        .status()
        .unwrap();
    let target = repository.path().join("AGENTS.md");
    let original = "# Rules\n";
    fs::write(&target, original).unwrap();

    let project = "instruction-local-apply-cas";
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
            "_rules/local-apply-cas.md",
            "--kind",
            "rule",
            "--body",
            "# CAS\n\nKeep the approved base hash stable.",
        ],
    );
    assert!(wrote.status.success());
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
            "_rules/local-apply-cas.md",
            "--target",
            "AGENTS.md",
            "--json",
        ],
    ));
    let proposal_id = proposal["proposal_id"].as_str().unwrap();
    let repository_before = snapshot(repository.path());

    let pending_apply = run(
        repository.path(),
        data.path(),
        &server.url,
        &[
            "instructions",
            "apply",
            proposal_id,
            "--workspace",
            "default",
            "--project",
            project,
            "--json",
        ],
    );
    assert!(!pending_apply.status.success());
    assert!(
        String::from_utf8_lossy(&pending_apply.stderr).contains("only approved"),
        "{}",
        String::from_utf8_lossy(&pending_apply.stderr)
    );
    assert_eq!(snapshot(repository.path()), repository_before);

    let approved = run(
        repository.path(),
        data.path(),
        &server.url,
        &[
            "pending-writes",
            "approve",
            proposal_id,
            "--workspace",
            "default",
            "--project",
            project,
        ],
    );
    assert!(approved.status.success());
    fs::write(&target, "# Rules\n\nconcurrent local change\n").unwrap();
    let mismatched_before = snapshot(repository.path());

    let mismatched_apply = run(
        repository.path(),
        data.path(),
        &server.url,
        &[
            "instructions",
            "apply",
            proposal_id,
            "--workspace",
            "default",
            "--project",
            project,
            "--json",
        ],
    );
    assert!(!mismatched_apply.status.success());
    assert!(
        String::from_utf8_lossy(&mismatched_apply.stderr)
            .contains("changed after proposal staging"),
        "{}",
        String::from_utf8_lossy(&mismatched_apply.stderr)
    );
    assert_eq!(snapshot(repository.path()), mismatched_before);
    assert_eq!(snapshot(&data.path().join("wiki")), wiki_before);

    let detail = json_success(run(
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
    assert!(detail["application"].is_null());
    assert!(
        !detail["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["event"] == "applied")
    );
}

#[test]
fn instruction_apply_rejects_a_different_repository_with_the_same_base() {
    let originating_repository = tempfile::tempdir().unwrap();
    let other_repository = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    for repository in [&originating_repository, &other_repository] {
        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(repository.path())
            .status()
            .unwrap();
        fs::write(repository.path().join("AGENTS.md"), "# Rules\n").unwrap();
    }

    let project = "instruction-repository-identity";
    let addr = reserve_addr();
    let server = Server::start(data.path(), project, &addr);
    assert!(
        run(
            originating_repository.path(),
            data.path(),
            &server.url,
            &[
                "write-page",
                "--workspace",
                "default",
                "--project",
                project,
                "--path",
                "_rules/repository-identity.md",
                "--kind",
                "rule",
                "--body",
                "# Repository identity\n\nApply only in the checkout that staged the proposal.",
            ],
        )
        .status
        .success()
    );
    let proposal = json_success(run(
        originating_repository.path(),
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
            "_rules/repository-identity.md",
            "--target",
            "AGENTS.md",
            "--json",
        ],
    ));
    let proposal_id = proposal["proposal_id"].as_str().unwrap();
    assert!(
        run(
            originating_repository.path(),
            data.path(),
            &server.url,
            &[
                "pending-writes",
                "approve",
                proposal_id,
                "--workspace",
                "default",
                "--project",
                project,
            ],
        )
        .status
        .success()
    );

    let originating_before = snapshot(originating_repository.path());
    let other_before = snapshot(other_repository.path());
    let applied_elsewhere = run(
        other_repository.path(),
        data.path(),
        &server.url,
        &[
            "instructions",
            "apply",
            proposal_id,
            "--workspace",
            "default",
            "--project",
            project,
            "--json",
        ],
    );
    assert!(!applied_elsewhere.status.success());
    assert!(
        String::from_utf8_lossy(&applied_elsewhere.stderr).contains("different repository"),
        "{}",
        String::from_utf8_lossy(&applied_elsewhere.stderr)
    );
    assert_eq!(snapshot(originating_repository.path()), originating_before);
    assert_eq!(snapshot(other_repository.path()), other_before);

    let detail = json_success(run(
        originating_repository.path(),
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
    assert!(detail["application"].is_null());
    assert_eq!(
        detail["repository_identity_sha256"].as_str().unwrap().len(),
        64
    );
}

#[test]
fn instruction_apply_recovers_a_written_but_unrecorded_update() {
    let repository = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(repository.path())
        .status()
        .unwrap();
    let original = "";
    let target = repository.path().join("AGENTS.md");
    fs::write(&target, original).unwrap();

    let project = "instruction-apply-recovery";
    let addr = reserve_addr();
    let server = Server::start(data.path(), project, &addr);
    assert!(
        run(
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
                "_rules/recovery.md",
                "--kind",
                "rule",
                "--body",
                "# Recovery\n\nRecord an already-written approved update on retry.",
            ],
        )
        .status
        .success()
    );
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
            "_rules/recovery.md",
            "--target",
            "AGENTS.md",
            "--json",
        ],
    ));
    let proposal_id = proposal["proposal_id"].as_str().unwrap();
    assert!(
        run(
            repository.path(),
            data.path(),
            &server.url,
            &[
                "pending-writes",
                "approve",
                proposal_id,
                "--workspace",
                "default",
                "--project",
                project,
            ],
        )
        .status
        .success()
    );
    let detail = json_success(run(
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
    assert_eq!(detail["base_target_existed"], true);
    let proposed = detail["proposed_content"].as_str().unwrap();
    let mut backup_name = target.as_os_str().to_owned();
    backup_name.push(".bak-1700000000");
    let backup = PathBuf::from(backup_name);
    fs::write(&backup, original).unwrap();
    fs::write(&target, proposed).unwrap();
    let repository_before_retry = snapshot(repository.path());

    let recovered = json_success(run(
        repository.path(),
        data.path(),
        &server.url,
        &[
            "instructions",
            "apply",
            proposal_id,
            "--workspace",
            "default",
            "--project",
            project,
            "--json",
        ],
    ));
    assert_eq!(recovered["status"], "applied");
    assert_eq!(recovered["outcome"], "updated");
    assert_eq!(recovered["idempotent"], false);
    assert_eq!(
        recovered["backup_path"],
        fs::canonicalize(&backup)
            .unwrap()
            .to_string_lossy()
            .as_ref()
    );
    assert_eq!(snapshot(repository.path()), repository_before_retry);
}

#[test]
fn instruction_apply_recovers_a_written_but_unrecorded_create() {
    let repository = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(repository.path())
        .status()
        .unwrap();
    let target = repository.path().join("AGENTS.md");
    assert!(!target.exists());

    let project = "instruction-create-recovery";
    let addr = reserve_addr();
    let server = Server::start(data.path(), project, &addr);
    assert!(
        run(
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
                "_rules/create-recovery.md",
                "--kind",
                "rule",
                "--body",
                "# Create recovery\n\nRecord an already-created approved target on retry.",
            ],
        )
        .status
        .success()
    );
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
            "_rules/create-recovery.md",
            "--target",
            "AGENTS.md",
            "--json",
        ],
    ));
    let proposal_id = proposal["proposal_id"].as_str().unwrap();
    assert!(
        run(
            repository.path(),
            data.path(),
            &server.url,
            &[
                "pending-writes",
                "approve",
                proposal_id,
                "--workspace",
                "default",
                "--project",
                project,
            ],
        )
        .status
        .success()
    );
    let detail = json_success(run(
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
    assert_eq!(detail["base_target_existed"], false);
    fs::write(&target, detail["proposed_content"].as_str().unwrap()).unwrap();
    let repository_before_retry = snapshot(repository.path());

    let recovered = json_success(run(
        repository.path(),
        data.path(),
        &server.url,
        &[
            "instructions",
            "apply",
            proposal_id,
            "--workspace",
            "default",
            "--project",
            project,
            "--json",
        ],
    ));
    assert_eq!(recovered["status"], "applied");
    assert_eq!(recovered["outcome"], "created");
    assert!(recovered["backup_path"].is_null());
    assert_eq!(snapshot(repository.path()), repository_before_retry);
}

#[test]
fn instruction_apply_rejects_move_operations_without_a_destination_write() {
    let repository = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(repository.path())
        .status()
        .unwrap();
    fs::write(
        repository.path().join("AGENTS.md"),
        "# Rules\n\n## Release procedure\n\n1. Build the release.\n2. Verify the signed artifact.\n3. Publish the release notes.\n",
    )
    .unwrap();

    let project = "instruction-move-rejected";
    let addr = reserve_addr();
    let server = Server::start(data.path(), project, &addr);
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
            "workflow_in_always_loaded_context",
            "--source",
            "AGENTS.md",
            "--line",
            "5",
            "--json",
        ],
    ));
    assert_eq!(proposal["operation"], "move_to_skill");
    let proposal_id = proposal["proposal_id"].as_str().unwrap();
    assert!(
        run(
            repository.path(),
            data.path(),
            &server.url,
            &[
                "pending-writes",
                "approve",
                proposal_id,
                "--workspace",
                "default",
                "--project",
                project,
            ],
        )
        .status
        .success()
    );
    let before = snapshot(repository.path());

    let apply = run(
        repository.path(),
        data.path(),
        &server.url,
        &[
            "instructions",
            "apply",
            proposal_id,
            "--workspace",
            "default",
            "--project",
            project,
            "--json",
        ],
    );
    assert!(!apply.status.success());
    assert!(
        String::from_utf8_lossy(&apply.stderr).contains("single-target local apply"),
        "{}",
        String::from_utf8_lossy(&apply.stderr)
    );
    assert_eq!(snapshot(repository.path()), before);
}

#[test]
fn instruction_apply_records_an_approved_no_change_without_writing() {
    let repository = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(repository.path())
        .status()
        .unwrap();
    fs::write(
        repository.path().join("AGENTS.md"),
        "# Rules\n\nUse rustfmt as this repository's coding convention.\n",
    )
    .unwrap();

    let project = "instruction-no-change";
    let addr = reserve_addr();
    let server = Server::start(data.path(), project, &addr);
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
            "protected_project_context",
            "--source",
            "AGENTS.md",
            "--line",
            "3",
            "--json",
        ],
    ));
    assert_eq!(proposal["operation"], "no_change");
    let proposal_id = proposal["proposal_id"].as_str().unwrap();
    let rejected_edit = run(
        repository.path(),
        data.path(),
        &server.url,
        &[
            "pending-writes",
            "edit",
            proposal_id,
            "--content",
            "# Changed despite no-change\n",
            "--workspace",
            "default",
            "--project",
            project,
            "--json",
        ],
    );
    assert!(!rejected_edit.status.success());
    assert!(
        String::from_utf8_lossy(&rejected_edit.stderr)
            .contains("must preserve the exact base content"),
        "{}",
        String::from_utf8_lossy(&rejected_edit.stderr)
    );
    assert!(
        run(
            repository.path(),
            data.path(),
            &server.url,
            &[
                "pending-writes",
                "approve",
                proposal_id,
                "--workspace",
                "default",
                "--project",
                project,
            ],
        )
        .status
        .success()
    );
    let before = snapshot(repository.path());

    let applied = json_success(run(
        repository.path(),
        data.path(),
        &server.url,
        &[
            "instructions",
            "apply",
            proposal_id,
            "--workspace",
            "default",
            "--project",
            project,
            "--json",
        ],
    ));
    assert_eq!(applied["outcome"], "no_op");
    assert!(applied["backup_path"].is_null());
    assert_eq!(snapshot(repository.path()), before);
}
