//! Black-box acceptance tests for instruction proposal and human-review stewardship.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use engram_core::{MARKER_END, MARKER_START};
use serde_json::Value;
use sha2::{Digest, Sha256};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_engram")
}

static RESERVED_ADDRS: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
static SERVER_SLOT: OnceLock<Mutex<()>> = OnceLock::new();

fn reserve_listener() -> (TcpListener, String) {
    let reserved = RESERVED_ADDRS.get_or_init(|| Mutex::new(BTreeSet::new()));
    loop {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        if reserved.lock().unwrap().insert(addr.clone()) {
            return (listener, addr);
        }
    }
}

fn reserve_addr() -> String {
    let (listener, addr) = reserve_listener();
    drop(listener);
    addr
}

struct Server {
    child: Child,
    url: String,
    _exclusive: std::sync::MutexGuard<'static, ()>,
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
        let exclusive = SERVER_SLOT
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
            .stderr(Stdio::null())
            .env_remove("ENGRAM_LLM_PROVIDER")
            .env_remove("ENGRAM_LLM_MODEL")
            .env_remove("ENGRAM_LLM_BASE_URL")
            .env_remove("ENGRAM_LLM_COMPAT_STRICT");
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
                    _exclusive: exclusive,
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
        let (listener, addr) = reserve_listener();
        listener.set_nonblocking(true).unwrap();
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

fn run_with_env(
    project: &Path,
    data_dir: &Path,
    server_url: &str,
    args: &[&str],
    key: &str,
    value: &str,
) -> Output {
    Command::new(bin())
        .args(["--data-dir", data_dir.to_str().unwrap()])
        .args(args)
        .current_dir(project)
        .env("ENGRAM_SERVER_URL", server_url)
        .env(key, value)
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

fn commit_all(repository: &Path, message: &str) {
    let added = Command::new("git")
        .args(["add", "--all"])
        .current_dir(repository)
        .status()
        .unwrap();
    assert!(added.success());
    let committed = Command::new("git")
        .args([
            "-c",
            "user.name=Engram Test",
            "-c",
            "user.email=engram-test@example.invalid",
            "commit",
            "--quiet",
            "-m",
            message,
        ])
        .current_dir(repository)
        .status()
        .unwrap();
    assert!(committed.success());
}

fn stage_and_approve_rule(
    repository: &Path,
    data_dir: &Path,
    server_url: &str,
    project: &str,
    rule_path: &str,
    rule_body: &str,
    target: &str,
) -> String {
    let wrote = run(
        repository,
        data_dir,
        server_url,
        &[
            "write-page",
            "--workspace",
            "default",
            "--project",
            project,
            "--path",
            rule_path,
            "--kind",
            "rule",
            "--body",
            rule_body,
        ],
    );
    assert!(
        wrote.status.success(),
        "{}",
        String::from_utf8_lossy(&wrote.stderr)
    );
    let proposal = json_success(run(
        repository,
        data_dir,
        server_url,
        &[
            "instructions",
            "propose",
            "--workspace",
            "default",
            "--project",
            project,
            "--rule",
            rule_path,
            "--target",
            target,
            "--json",
        ],
    ));
    let proposal_id = proposal["proposal_id"].as_str().unwrap().to_owned();
    let approved = run(
        repository,
        data_dir,
        server_url,
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
    );
    assert!(
        approved.status.success(),
        "{}",
        String::from_utf8_lossy(&approved.stderr)
    );
    proposal_id
}

fn assert_apply_failure_audit(
    repository: &Path,
    data_dir: &Path,
    server_url: &str,
    project: &str,
    proposal_id: &str,
    expected: (&str, &str, &str),
) {
    let (status, event, code) = expected;
    let detail = json_success(run(
        repository,
        data_dir,
        server_url,
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
    assert_eq!(detail["summary"]["status"], status);
    assert!(detail["application"].is_null());
    let matching: Vec<_> = detail["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["event"] == event && entry["detail_json"]["code"] == code)
        .collect();
    assert_eq!(matching.len(), 1, "expected one {event}/{code} audit event");
    assert!(
        !detail["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["event"] == "applied"),
        "failed local apply must never record a false applied event"
    );
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
fn doctor_to_local_apply_preserves_protected_context_and_git_index() {
    let repository = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(repository.path())
        .status()
        .unwrap();
    let original = "# Project instructions\n\n\
Think step by step and be helpful.\n\n\
- Rust filesystem errors in this repository must use `anyhow::Context`.\n\
- Production deployment uses the private `opsctl release` workflow.\n\
- The internal `artifact-index` tool is the only supported index writer.\n\
- Database migrations must remain backward-compatible for one release.\n\
- Enterprise tenants must never receive consumer trial entitlements.\n\
- Authentication checks must never be bypassed.\n";
    let target = repository.path().join("AGENTS.md");
    fs::write(&target, original).unwrap();
    commit_all(repository.path(), "initial project instructions");
    fs::write(
        repository.path().join("NOTES.md"),
        "unrelated staged bytes\n",
    )
    .unwrap();
    Command::new("git")
        .args(["add", "NOTES.md"])
        .current_dir(repository.path())
        .status()
        .unwrap();
    let git_index_before = fs::read(repository.path().join(".git/index")).unwrap();

    let project = "instruction-maintenance-workflow";
    let addr = reserve_addr();
    let server = Server::start(data.path(), project, &addr);
    let doctor = json_success(run(
        repository.path(),
        data.path(),
        &server.url,
        &["instructions", "doctor", "--json"],
    ));
    assert_eq!(doctor["canonical"]["path"], "AGENTS.md");
    let findings = doctor["placement_findings"].as_array().unwrap();
    let generic = findings
        .iter()
        .find(|finding| finding["category"] == "generic_harness")
        .unwrap();
    assert_eq!(generic["action"], "remove");
    assert_eq!(generic["protected"], false);
    for category in [
        "team_convention",
        "private_deployment",
        "internal_tool",
        "database_migration",
        "business_boundary",
        "security_requirement",
    ] {
        let protected = findings
            .iter()
            .find(|finding| finding["category"] == category)
            .unwrap_or_else(|| panic!("missing protected category {category}"));
        assert_eq!(protected["protected"], true, "{category}");
        assert_ne!(protected["action"], "remove", "{category}");
    }

    let repository_before_proposal = snapshot(repository.path());
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
    assert_eq!(proposal["status"], "pending");
    assert_eq!(proposal["operation"], "stale_delete");
    assert_eq!(snapshot(repository.path()), repository_before_proposal);

    let shown = json_success(run(
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
    assert_eq!(shown["summary"]["status"], "pending");
    assert_eq!(shown["summary"]["target_kind"], "project_instruction");
    assert_eq!(shown["provenance"][0]["kind"], "doctor_finding");
    assert_eq!(
        shown["base_sha256"],
        sha256_hex(original.as_bytes()),
        "the proposal must be CAS-bound to the reviewed target"
    );
    let proposed = shown["proposed_content"].as_str().unwrap().to_owned();
    assert!(!proposed.contains("Think step by step"));
    for protected in [
        "anyhow::Context",
        "opsctl release",
        "artifact-index",
        "backward-compatible",
        "consumer trial entitlements",
        "Authentication checks",
    ] {
        assert!(
            proposed.contains(protected),
            "protected repository knowledge was lost: {protected}"
        );
    }
    let diff = json_success(run(
        repository.path(),
        data.path(),
        &server.url,
        &[
            "pending-writes",
            "diff",
            &proposal_id,
            "--workspace",
            "default",
            "--project",
            project,
            "--json",
        ],
    ));
    let unified = diff["diff"].as_str().unwrap();
    assert!(unified.contains("-Think step by step and be helpful."));
    assert_eq!(snapshot(repository.path()), repository_before_proposal);

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
    assert_eq!(applied["status"], "applied");
    assert_eq!(applied["outcome"], "updated");
    assert_eq!(applied["idempotent"], false);
    assert_eq!(applied["before_sha256"], sha256_hex(original.as_bytes()));
    assert_eq!(applied["after_sha256"], sha256_hex(proposed.as_bytes()));
    let backup = PathBuf::from(applied["backup_path"].as_str().unwrap());
    assert_eq!(fs::read_to_string(&backup).unwrap(), original);
    assert_eq!(fs::read_to_string(&target).unwrap(), proposed);
    assert_eq!(
        fs::read(repository.path().join(".git/index")).unwrap(),
        git_index_before,
        "local apply must not stage either its target or unrelated work"
    );

    let after_first_apply = snapshot(repository.path());
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
    assert_eq!(snapshot(repository.path()), after_first_apply);

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
    let application = &detail["application"];
    assert!(application["proposing_actor"].is_object());
    assert!(application["approving_actor"].is_object());
    assert!(application["applying_actor"].is_object());
    assert_eq!(
        detail["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["event"] == "applied")
            .count(),
        1,
        "idempotent retries must not duplicate the terminal audit event"
    );
    let status = Command::new("git")
        .args(["status", "--short"])
        .current_dir(repository.path())
        .output()
        .unwrap();
    assert!(status.status.success());
    let status = String::from_utf8(status.stdout).unwrap();
    let actual_status: BTreeSet<_> = status.lines().map(str::to_owned).collect();
    let repository_root = fs::canonicalize(repository.path()).unwrap();
    let backup_root = fs::canonicalize(backup.parent().unwrap())
        .unwrap()
        .strip_prefix(repository_root)
        .unwrap()
        .to_owned();
    let expected_status = BTreeSet::from([
        " M AGENTS.md".to_owned(),
        "A  NOTES.md".to_owned(),
        format!("?? {}/", backup_root.display()),
    ]);
    assert_eq!(
        actual_status, expected_status,
        "local apply must create no unrelated Git-visible changes"
    );
    assert_eq!(
        fs::read(repository.path().join("NOTES.md")).unwrap(),
        b"unrelated staged bytes\n",
        "local apply must preserve unrelated file bytes"
    );
}

#[test]
fn installed_maintenance_skill_guides_llm_disabled_explicit_rule_apply() {
    let repository = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(repository.path())
        .status()
        .unwrap();
    fs::write(repository.path().join("README.md"), "# Seed\n").unwrap();
    commit_all(repository.path(), "initial repository");

    let installed = run(
        repository.path(),
        data.path(),
        "http://127.0.0.1:1",
        &["install-instructions", "--target", "AGENTS.md"],
    );
    assert!(
        installed.status.success(),
        "{}",
        String::from_utf8_lossy(&installed.stderr)
    );
    let instruction = fs::read_to_string(repository.path().join("AGENTS.md")).unwrap();
    assert!(instruction.contains("project-instruction maintenance"));
    assert!(!instruction.contains("instructions propose"));
    let skill_path = repository
        .path()
        .join(".agents/skills/engram-project-instruction-maintenance/SKILL.md");
    let skill = fs::read_to_string(&skill_path).unwrap();
    for command in [
        "engram instructions doctor",
        "engram instructions propose",
        "engram pending-writes diff",
        "engram pending-writes approve",
        "engram instructions apply",
    ] {
        assert!(
            skill.contains(command),
            "missing workflow command {command}"
        );
    }
    assert!(skill.contains("explicit human"));
    assert!(skill.contains("local host"));
    assert!(!repository.path().join(".claude/skills").exists());
    commit_all(repository.path(), "install managed maintenance workflow");
    let git_index_before = fs::read(repository.path().join(".git/index")).unwrap();

    let project = "instruction-llm-disabled";
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
            "_rules/manual-review.md",
            "--kind",
            "rule",
            "--body",
            "# Manual review\n\nRequire explicit human approval before local instruction apply.",
        ],
    );
    assert!(wrote.status.success());

    let target = repository.path().join("AGENTS.md");
    let base = fs::read_to_string(&target).unwrap();
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
            "_rules/manual-review.md",
            "--target",
            "AGENTS.md",
            "--json",
        ],
    ));
    let proposal_id = proposal["proposal_id"].as_str().unwrap().to_owned();
    assert_eq!(proposal["status"], "pending");
    assert!(proposal["semantic_assistance"].is_null());
    assert_eq!(fs::read_to_string(&target).unwrap(), base);

    let shown = json_success(run(
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
    assert_eq!(shown["provenance"][0]["kind"], "durable_rule");
    assert_eq!(shown["provenance"][0]["source"], "_rules/manual-review.md");
    assert_eq!(shown["provenance"][0]["selection"], "explicit_cli");
    assert!(
        shown["proposed_content"]
            .as_str()
            .unwrap()
            .contains("Require explicit human approval")
    );

    let unapproved_apply = run(
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
    );
    assert!(!unapproved_apply.status.success());
    assert!(
        String::from_utf8_lossy(&unapproved_apply.stderr).contains("only approved"),
        "{}",
        String::from_utf8_lossy(&unapproved_apply.stderr)
    );
    assert_eq!(fs::read_to_string(&target).unwrap(), base);

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
    assert_eq!(fs::read_to_string(&target).unwrap(), base);

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
    assert_eq!(applied["status"], "applied");
    assert_eq!(applied["outcome"], "updated");
    let backup = PathBuf::from(applied["backup_path"].as_str().unwrap());
    assert_eq!(fs::read_to_string(backup).unwrap(), base);
    assert_eq!(
        fs::read(repository.path().join(".git/index")).unwrap(),
        git_index_before
    );
    let first_apply = snapshot(repository.path());
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
    assert_eq!(repeated["idempotent"], true);
    assert_eq!(snapshot(repository.path()), first_apply);
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
    commit_all(repository.path(), "initial instructions");
    fs::write(
        repository.path().join("NOTES.md"),
        "unrelated staged bytes\n",
    )
    .unwrap();
    Command::new("git")
        .args(["add", "NOTES.md"])
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
fn approved_add_creates_a_missing_target_without_touching_the_git_index() {
    let repository = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(repository.path())
        .status()
        .unwrap();
    fs::write(repository.path().join("README.md"), "# Seed\n").unwrap();
    commit_all(repository.path(), "initial repository");
    let index_before = fs::read(repository.path().join(".git/index")).unwrap();

    let project = "instruction-create-missing";
    let addr = reserve_addr();
    let server = Server::start(data.path(), project, &addr);
    let proposal_id = stage_and_approve_rule(
        repository.path(),
        data.path(),
        &server.url,
        project,
        "_rules/create.md",
        "# Create\n\nCreate the approved instruction target.",
        "AGENTS.md",
    );
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
    assert_eq!(detail["base_target_existed"], false);
    let expected = detail["proposed_content"].as_str().unwrap();

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
    assert_eq!(applied["status"], "applied");
    assert_eq!(applied["outcome"], "created");
    assert!(applied["backup_path"].is_null());
    assert_eq!(
        fs::read_to_string(repository.path().join("AGENTS.md")).unwrap(),
        expected
    );
    assert_eq!(
        fs::read(repository.path().join(".git/index")).unwrap(),
        index_before
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
    commit_all(repository.path(), "initial instructions");

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
    assert_eq!(detail["summary"]["status"], "conflict");
    assert_eq!(
        detail["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| {
                event["event"] == "conflict" && event["detail_json"]["code"] == "target_changed"
            })
            .count(),
        1
    );
    assert!(
        !detail["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["event"] == "applied")
    );
}

#[test]
fn instruction_apply_rejects_a_dirty_target_even_when_its_bytes_match_the_approved_base() {
    let repository = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(repository.path())
        .status()
        .unwrap();
    let target = repository.path().join("AGENTS.md");
    fs::write(&target, "# Rules\n").unwrap();
    commit_all(repository.path(), "initial instructions");
    fs::write(&target, "# Rules\n\nLocally staged owner bytes.\n").unwrap();
    assert!(
        Command::new("git")
            .args(["add", "AGENTS.md"])
            .current_dir(repository.path())
            .status()
            .unwrap()
            .success()
    );

    let project = "instruction-dirty-target";
    let addr = reserve_addr();
    let server = Server::start(data.path(), project, &addr);
    let proposal_id = stage_and_approve_rule(
        repository.path(),
        data.path(),
        &server.url,
        project,
        "_rules/dirty-target.md",
        "# Dirty target\n\nNever overwrite a dirty instruction target.",
        "AGENTS.md",
    );
    let before = snapshot(repository.path());
    let index_before = fs::read(repository.path().join(".git/index")).unwrap();

    let applied = run(
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
    );
    assert!(!applied.status.success());
    assert!(
        String::from_utf8_lossy(&applied.stderr).contains("dirty instruction target"),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    assert_eq!(snapshot(repository.path()), before);
    assert_eq!(
        fs::read(repository.path().join(".git/index")).unwrap(),
        index_before
    );
    assert_apply_failure_audit(
        repository.path(),
        data.path(),
        &server.url,
        project,
        &proposal_id,
        ("conflict", "conflict", "dirty_instruction_target"),
    );
}

#[test]
fn instruction_apply_rejects_an_ambiguous_git_operation_state_without_mutation() {
    let repository = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(repository.path())
        .status()
        .unwrap();
    fs::write(repository.path().join("AGENTS.md"), "# Rules\n").unwrap();
    commit_all(repository.path(), "initial instructions");

    let project = "instruction-merge-state";
    let addr = reserve_addr();
    let server = Server::start(data.path(), project, &addr);
    let proposal_id = stage_and_approve_rule(
        repository.path(),
        data.path(),
        &server.url,
        project,
        "_rules/merge-state.md",
        "# Merge state\n\nRequire a clean Git operation state before applying.",
        "AGENTS.md",
    );
    let head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repository.path())
        .output()
        .unwrap();
    assert!(head.status.success());
    fs::write(repository.path().join(".git/MERGE_HEAD"), head.stdout).unwrap();
    let before = snapshot(repository.path());

    let applied = run(
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
    );
    assert!(!applied.status.success());
    assert!(
        String::from_utf8_lossy(&applied.stderr).contains("ambiguous Git operation state"),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    assert_eq!(snapshot(repository.path()), before);
    assert_apply_failure_audit(
        repository.path(),
        data.path(),
        &server.url,
        project,
        &proposal_id,
        ("conflict", "conflict", "ambiguous_git_state"),
    );
}

#[test]
fn instruction_apply_rejects_every_malformed_managed_marker_shape() {
    const APPROVED_START: &str = "<!-- engram:approved-rules:start -->";
    const APPROVED_END: &str = "<!-- engram:approved-rules:end -->";
    let cases = [
        (
            "routing_missing_end",
            format!("# Rules\n\n{MARKER_START}\nbroken\n"),
        ),
        (
            "routing_duplicate",
            format!(
                "# Rules\n\n{MARKER_START}\none\n{MARKER_END}\n{MARKER_START}\ntwo\n{MARKER_END}\n"
            ),
        ),
        (
            "routing_nested",
            format!(
                "# Rules\n\n{MARKER_START}\n{MARKER_START}\nnested\n{MARKER_END}\n{MARKER_END}\n"
            ),
        ),
        (
            "routing_crossed",
            format!("# Rules\n\n{MARKER_END}\ncrossed\n{MARKER_START}\n"),
        ),
        (
            "approved_missing_end",
            format!("# Rules\n\n{APPROVED_START}\nbroken\n"),
        ),
        (
            "approved_duplicate",
            format!(
                "# Rules\n\n{APPROVED_START}\none\n{APPROVED_END}\n{APPROVED_START}\ntwo\n{APPROVED_END}\n"
            ),
        ),
        (
            "approved_nested",
            format!(
                "# Rules\n\n{APPROVED_START}\n{APPROVED_START}\nnested\n{APPROVED_END}\n{APPROVED_END}\n"
            ),
        ),
        (
            "approved_crossed",
            format!("# Rules\n\n{APPROVED_END}\ncrossed\n{APPROVED_START}\n"),
        ),
        (
            "crossed_domains",
            format!(
                "# Rules\n\n{MARKER_START}\n{APPROVED_START}\ncrossed\n{MARKER_END}\n{APPROVED_END}\n"
            ),
        ),
        (
            "nested_domains",
            format!(
                "# Rules\n\n{APPROVED_START}\n{MARKER_START}\nnested\n{MARKER_END}\n{APPROVED_END}\n"
            ),
        ),
    ];

    for (case, original) in cases {
        let repository = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(repository.path())
            .status()
            .unwrap();
        let target = repository.path().join("AGENTS.md");
        fs::write(&target, &original).unwrap();
        commit_all(repository.path(), "initial malformed instructions");
        let project = format!("instruction-marker-{case}");
        let addr = reserve_addr();
        let server = Server::start(data.path(), &project, &addr);
        let proposal_id = stage_and_approve_rule(
            repository.path(),
            data.path(),
            &server.url,
            &project,
            "_rules/marker-safety.md",
            "# Marker safety\n\nReject malformed managed marker structure.",
            "AGENTS.md",
        );
        let before = snapshot(repository.path());

        let applied = run(
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
                &project,
                "--json",
            ],
        );
        assert!(
            !applied.status.success(),
            "case {case} unexpectedly applied"
        );
        assert!(
            String::from_utf8_lossy(&applied.stderr).contains("managed markers are malformed"),
            "case {case}: {}",
            String::from_utf8_lossy(&applied.stderr)
        );
        assert_eq!(snapshot(repository.path()), before, "case {case}");
        assert_apply_failure_audit(
            repository.path(),
            data.path(),
            &server.url,
            &project,
            &proposal_id,
            ("failed", "failed", "malformed_markers"),
        );
    }
}

#[cfg(unix)]
#[test]
fn instruction_apply_resolves_safe_symlink_and_preserves_bytes_mode_newlines_and_index() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    const APPROVED_START: &str = "<!-- engram:approved-rules:start -->";
    const APPROVED_END: &str = "<!-- engram:approved-rules:end -->";
    let repository = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(repository.path())
        .status()
        .unwrap();
    fs::create_dir_all(repository.path().join("docs")).unwrap();
    let canonical = repository.path().join("docs/AGENTS-main.md");
    let original = format!(
        "# Rules\r\n\r\nHuman prefix.\r\n\r\n{MARKER_START}\r\nrouting bytes\r\n{MARKER_END}\r\n\r\n{APPROVED_START}\r\napproved bytes\r\n{APPROVED_END}\r\n\r\nHuman tail.\r\n"
    );
    fs::write(&canonical, &original).unwrap();
    fs::set_permissions(&canonical, fs::Permissions::from_mode(0o640)).unwrap();
    symlink("docs/AGENTS-main.md", repository.path().join("AGENTS.md")).unwrap();
    commit_all(repository.path(), "initial symlinked instructions");
    fs::write(
        repository.path().join("NOTES.md"),
        "unrelated staged bytes\n",
    )
    .unwrap();
    assert!(
        Command::new("git")
            .args(["add", "NOTES.md"])
            .current_dir(repository.path())
            .status()
            .unwrap()
            .success()
    );
    let index_before = fs::read(repository.path().join(".git/index")).unwrap();

    let project = "instruction-safe-symlink";
    let addr = reserve_addr();
    let server = Server::start(data.path(), project, &addr);
    let proposal_id = stage_and_approve_rule(
        repository.path(),
        data.path(),
        &server.url,
        project,
        "_rules/safe-symlink.md",
        "# Safe symlink\n\nPreserve owner bytes through the canonical adapter.",
        "AGENTS.md",
    );
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
    assert_eq!(applied["status"], "applied");
    assert!(
        fs::symlink_metadata(repository.path().join("AGENTS.md"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let expected = format!("{original}\r\nPreserve owner bytes through the canonical adapter.\r\n");
    let actual = fs::read_to_string(&canonical).unwrap();
    assert_eq!(actual, expected);
    assert!(!actual.replace("\r\n", "").contains('\n'));
    assert_eq!(
        fs::metadata(&canonical).unwrap().permissions().mode() & 0o777,
        0o640
    );
    assert_eq!(
        fs::read(repository.path().join(".git/index")).unwrap(),
        index_before
    );
}

#[cfg(unix)]
#[test]
fn instruction_apply_rejects_a_cleanly_retargeted_symlink() {
    use std::os::unix::fs::symlink;

    let repository = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(repository.path())
        .status()
        .unwrap();
    fs::write(repository.path().join("one.md"), "# Rules\n").unwrap();
    fs::write(repository.path().join("two.md"), "# Rules\n").unwrap();
    symlink("one.md", repository.path().join("AGENTS.md")).unwrap();
    commit_all(repository.path(), "initial safe symlink");

    let project = "instruction-retargeted-symlink";
    let addr = reserve_addr();
    let server = Server::start(data.path(), project, &addr);
    let proposal_id = stage_and_approve_rule(
        repository.path(),
        data.path(),
        &server.url,
        project,
        "_rules/retarget.md",
        "# Retarget\n\nNever follow a post-approval symlink retarget.",
        "AGENTS.md",
    );
    fs::remove_file(repository.path().join("AGENTS.md")).unwrap();
    symlink("two.md", repository.path().join("AGENTS.md")).unwrap();
    commit_all(repository.path(), "retarget symlink after approval");
    let before = snapshot(repository.path());
    let applied = run(
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
    );
    assert!(!applied.status.success());
    assert_eq!(snapshot(repository.path()), before);
    assert_apply_failure_audit(
        repository.path(),
        data.path(),
        &server.url,
        project,
        &proposal_id,
        ("conflict", "conflict", "different_repository"),
    );
}

#[test]
fn instruction_apply_follows_safe_import_without_writing_the_adapter() {
    let repository = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(repository.path())
        .status()
        .unwrap();
    let adapter = "# Claude adapter\n\n@AGENTS.md\n";
    fs::write(repository.path().join("CLAUDE.md"), adapter).unwrap();
    fs::write(repository.path().join("AGENTS.md"), "# Rules\n").unwrap();
    commit_all(repository.path(), "initial imported instructions");

    let project = "instruction-safe-import";
    let addr = reserve_addr();
    let server = Server::start(data.path(), project, &addr);
    let proposal_id = stage_and_approve_rule(
        repository.path(),
        data.path(),
        &server.url,
        project,
        "_rules/safe-import.md",
        "# Safe import\n\nWrite only the canonical imported source.",
        "AGENTS.md",
    );
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
    assert_eq!(applied["status"], "applied");
    assert_eq!(
        fs::read_to_string(repository.path().join("CLAUDE.md")).unwrap(),
        adapter
    );
    assert_eq!(
        fs::read_to_string(repository.path().join("AGENTS.md")).unwrap(),
        "# Rules\n\nWrite only the canonical imported source.\n"
    );
}

#[cfg(unix)]
#[test]
fn instruction_apply_blocks_unsafe_symlink_encoding_and_import_graphs() {
    use std::os::unix::fs::symlink;

    let cases = [
        ("unsupported_encoding", "unsupported_encoding", "conflict"),
        ("unsafe_symlink", "unsafe_symlink", "conflict"),
        ("unresolved_import", "unresolved_import", "failed"),
        ("import_cycle", "import_cycle", "failed"),
        ("escaped_import", "escaped_import", "failed"),
    ];
    for (case, expected_code, expected_status) in cases {
        let repository = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(repository.path())
            .status()
            .unwrap();
        let target = repository.path().join("AGENTS.md");
        fs::write(&target, "# Rules\n").unwrap();
        commit_all(repository.path(), "initial instructions");
        let project = format!("instruction-hazard-{case}");
        let addr = reserve_addr();
        let server = Server::start(data.path(), &project, &addr);
        let proposal_id = stage_and_approve_rule(
            repository.path(),
            data.path(),
            &server.url,
            &project,
            "_rules/hazard.md",
            "# Hazard\n\nRun every safety preflight before mutation.",
            "AGENTS.md",
        );

        let outside = tempfile::tempdir().unwrap();
        let outside_target = outside.path().join("outside.md");
        fs::write(&outside_target, "# Rules\n").unwrap();
        match case {
            "unsupported_encoding" => fs::write(&target, [0xff, 0xfe, b'X']).unwrap(),
            "unsafe_symlink" => {
                fs::remove_file(&target).unwrap();
                symlink(&outside_target, &target).unwrap();
            }
            "unresolved_import" => {
                fs::write(repository.path().join("CLAUDE.md"), "@missing.md\n").unwrap();
            }
            "import_cycle" => {
                fs::write(repository.path().join("CLAUDE.md"), "@rules.md\n").unwrap();
                fs::write(repository.path().join("rules.md"), "@CLAUDE.md\n").unwrap();
            }
            "escaped_import" => {
                fs::write(repository.path().join("CLAUDE.md"), "@../outside.md\n").unwrap();
            }
            _ => unreachable!(),
        }
        let before = snapshot(repository.path());
        let outside_before = fs::read(&outside_target).unwrap();
        let applied = run(
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
                &project,
                "--json",
            ],
        );
        assert!(
            !applied.status.success(),
            "case {case} unexpectedly applied"
        );
        assert_eq!(snapshot(repository.path()), before, "case {case}");
        assert_eq!(
            fs::read(&outside_target).unwrap(),
            outside_before,
            "case {case}"
        );
        assert_apply_failure_audit(
            repository.path(),
            data.path(),
            &server.url,
            &project,
            &proposal_id,
            (expected_status, expected_status, expected_code),
        );
    }
}

#[test]
fn instruction_apply_rejects_an_ambiguous_line_anchor() {
    let repository = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(repository.path())
        .status()
        .unwrap();
    let repeated = "Think step by step and be helpful.\n";
    fs::write(
        repository.path().join("AGENTS.md"),
        format!("# Rules\n\n{repeated}\n{repeated}"),
    )
    .unwrap();
    commit_all(repository.path(), "initial repeated anchor");
    let project = "instruction-ambiguous-anchor";
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
            "generic_harness_guidance",
            "--source",
            "AGENTS.md",
            "--line",
            "3",
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
    let before = snapshot(repository.path());
    let applied = run(
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
    assert!(!applied.status.success());
    assert_eq!(snapshot(repository.path()), before);
    assert_apply_failure_audit(
        repository.path(),
        data.path(),
        &server.url,
        project,
        proposal_id,
        ("failed", "failed", "ambiguous_anchor"),
    );
}

#[test]
fn instruction_proposal_cannot_target_managed_skill_paths() {
    let repository = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(repository.path())
        .status()
        .unwrap();
    fs::create_dir_all(repository.path().join(".agents/skills/example")).unwrap();
    let target = repository.path().join(".agents/skills/example/SKILL.md");
    fs::write(&target, "<!-- engram-managed: routing-skill -->\n# Skill\n").unwrap();
    fs::write(repository.path().join("AGENTS.md"), "# Rules\n").unwrap();
    commit_all(repository.path(), "initial managed skill");
    let project = "instruction-managed-skill";
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
                "_rules/skill.md",
                "--kind",
                "rule",
                "--body",
                "# Skill\n\nNever mutate managed skills through instruction apply.",
            ],
        )
        .status
        .success()
    );
    let before = snapshot(repository.path());
    let proposed = run(
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
            "_rules/skill.md",
            "--target",
            ".agents/skills/example/SKILL.md",
            "--json",
        ],
    );
    assert!(!proposed.status.success());
    assert!(String::from_utf8_lossy(&proposed.stderr).contains("managed Agent Skill"));
    assert_eq!(snapshot(repository.path()), before);

    let outside = tempfile::NamedTempFile::new().unwrap();
    fs::write(outside.path(), "outside owner bytes\n").unwrap();
    let outside_before = fs::read(outside.path()).unwrap();
    let escaped = run(
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
            "_rules/skill.md",
            "--target",
            "../outside.md",
            "--json",
        ],
    );
    assert!(!escaped.status.success());
    assert!(String::from_utf8_lossy(&escaped.stderr).contains("invalid repository-relative"));
    assert_eq!(fs::read(outside.path()).unwrap(), outside_before);
}

#[test]
fn routing_refresh_cli_preserves_approved_rules_and_rejects_malformed_markers() {
    const APPROVED_START: &str = "<!-- engram:approved-rules:start -->";
    const APPROVED_END: &str = "<!-- engram:approved-rules:end -->";
    let repository = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(repository.path())
        .status()
        .unwrap();
    let approved = format!("{APPROVED_START}\napproved owner bytes\n{APPROVED_END}");
    let target = repository.path().join("AGENTS.md");
    fs::write(
        &target,
        format!(
            "# Rules\n\n{approved}\n\n{MARKER_START}\nstale routing\n{MARKER_END}\n\nHuman tail.\n"
        ),
    )
    .unwrap();
    commit_all(repository.path(), "initial routing instructions");
    let index_before = fs::read(repository.path().join(".git/index")).unwrap();
    let refreshed = run(
        repository.path(),
        data.path(),
        "http://127.0.0.1:9",
        &[
            "install-instructions",
            "--target",
            "AGENTS.md",
            "--no-skills",
        ],
    );
    assert!(
        refreshed.status.success(),
        "{}",
        String::from_utf8_lossy(&refreshed.stderr)
    );
    let current = fs::read_to_string(&target).unwrap();
    assert!(current.contains(&approved));
    assert!(current.contains("Human tail."));
    assert!(!current.contains("stale routing"));
    assert_eq!(
        fs::read(repository.path().join(".git/index")).unwrap(),
        index_before
    );

    let malformed = format!(
        "# Rules\n\n{approved}\n\n{MARKER_START}\none\n{MARKER_END}\n{MARKER_START}\ntwo\n{MARKER_END}\n"
    );
    fs::write(&target, &malformed).unwrap();
    let before = snapshot(repository.path());
    let rejected = run(
        repository.path(),
        data.path(),
        "http://127.0.0.1:9",
        &[
            "install-instructions",
            "--target",
            "AGENTS.md",
            "--no-skills",
        ],
    );
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("managed markers are malformed"));
    assert_eq!(snapshot(repository.path()), before);
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
fn instruction_apply_rejects_forged_backup_without_a_bound_receipt() {
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
    commit_all(repository.path(), "initial empty instructions");

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

    let recovered = run(
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
    assert!(!recovered.status.success());
    assert!(
        String::from_utf8_lossy(&recovered.stderr).contains("local apply receipt"),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert_eq!(snapshot(repository.path()), repository_before_retry);
    assert_apply_failure_audit(
        repository.path(),
        data.path(),
        &server.url,
        project,
        proposal_id,
        ("conflict", "conflict", "target_changed"),
    );
}

#[test]
fn instruction_apply_rejects_matching_created_bytes_without_a_bound_receipt() {
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

    let recovered = run(
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
    assert!(!recovered.status.success());
    assert!(
        String::from_utf8_lossy(&recovered.stderr).contains("local apply receipt"),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert_eq!(snapshot(repository.path()), repository_before_retry);
    assert_apply_failure_audit(
        repository.path(),
        data.path(),
        &server.url,
        project,
        proposal_id,
        ("conflict", "conflict", "target_changed"),
    );
}

#[test]
fn instruction_apply_rejects_a_preexisting_receipt_before_mutation() {
    let repository = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(repository.path())
        .status()
        .unwrap();
    let target = repository.path().join("AGENTS.md");
    fs::write(&target, "# Rules\n").unwrap();
    commit_all(repository.path(), "initial instructions");

    let project = "instruction-preexisting-receipt";
    let addr = reserve_addr();
    let server = Server::start(data.path(), project, &addr);
    let proposal_id = stage_and_approve_rule(
        repository.path(),
        data.path(),
        &server.url,
        project,
        "_rules/receipt.md",
        "# Receipt\n\nReserve audit state before mutation.",
        "AGENTS.md",
    );
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
    let approval = detail["approval_sha256"].as_str().unwrap();
    let receipt_dir = repository.path().join(".git/engram-local-apply");
    fs::create_dir_all(&receipt_dir).unwrap();
    fs::write(receipt_dir.join("hmac-key"), [9_u8; 32]).unwrap();
    let receipt = receipt_dir.join(format!(
        "{}-{approval}.json",
        sha256_hex(proposal_id.as_bytes())
    ));
    fs::write(&receipt, "preexisting").unwrap();
    let before = snapshot(repository.path());
    let applied = run(
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
    );
    assert!(!applied.status.success());
    assert_eq!(snapshot(repository.path()), before);
    assert_apply_failure_audit(
        repository.path(),
        data.path(),
        &server.url,
        project,
        &proposal_id,
        ("failed", "failed", "local_receipt_failed"),
    );
}

#[test]
fn instruction_apply_rolls_back_when_receipt_finalization_fails() {
    let repository = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(repository.path())
        .status()
        .unwrap();
    let target = repository.path().join("AGENTS.md");
    let base = "# Rules\n";
    fs::write(&target, base).unwrap();
    commit_all(repository.path(), "initial instructions");
    let index_before = fs::read(repository.path().join(".git/index")).unwrap();

    let project = "instruction-receipt-rollback";
    let addr = reserve_addr();
    let server = Server::start(data.path(), project, &addr);
    let proposal_id = stage_and_approve_rule(
        repository.path(),
        data.path(),
        &server.url,
        project,
        "_rules/rollback.md",
        "# Rollback\n\nRestore the base if receipt finalization fails.",
        "AGENTS.md",
    );
    let applied = run_with_env(
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
        "ENGRAM_TEST_FAIL_RECEIPT_FINALIZE",
        "1",
    );
    assert!(!applied.status.success());
    assert_eq!(fs::read_to_string(&target).unwrap(), base);
    assert_eq!(
        fs::read(repository.path().join(".git/index")).unwrap(),
        index_before
    );
    assert_apply_failure_audit(
        repository.path(),
        data.path(),
        &server.url,
        project,
        &proposal_id,
        ("failed", "failed", "local_receipt_failed"),
    );
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
    commit_all(repository.path(), "initial instructions");

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
