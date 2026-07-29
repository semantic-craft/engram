#![cfg(unix)]

//! Black-box regression tests for the operator-facing release script.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

fn run_git(repo: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("run git")
}

#[test]
fn release_moves_plain_unreleased_section_into_versioned_entry() {
    let fixture = tempfile::tempdir().expect("release fixture");
    let repo = fixture.path();
    let bin_dir = repo.join("bin");
    let fake_bin = repo.join("fake-bin");
    fs::create_dir_all(&bin_dir).expect("create bin directory");
    fs::create_dir_all(&fake_bin).expect("create fake-bin directory");

    let release_source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bin/release");
    let release = bin_dir.join("release");
    fs::copy(release_source, &release).expect("copy release script");
    fs::set_permissions(&release, fs::Permissions::from_mode(0o755))
        .expect("make release script executable");

    let fake_cargo = fake_bin.join("cargo");
    fs::write(&fake_cargo, "#!/bin/sh\nexit 0\n").expect("write fake cargo");
    fs::set_permissions(&fake_cargo, fs::Permissions::from_mode(0o755))
        .expect("make fake cargo executable");

    fs::write(
        repo.join("Cargo.toml"),
        "[workspace]\n\n[workspace.package]\nversion = \"2.0.0\"\n",
    )
    .expect("write Cargo.toml");
    fs::write(repo.join("Cargo.lock"), "# fixture lockfile\n").expect("write Cargo.lock");
    fs::write(
        repo.join("CHANGELOG.md"),
        "# Changelog\n\n## Unreleased\n\n### Fixed\n\n- Corrected release notes.\n\n## 2.0.0 - 2026-07-19\n\n- Previous release.\n",
    )
    .expect("write changelog");

    assert!(run_git(repo, &["init", "-b", "main"]).status.success());
    assert!(
        run_git(repo, &["config", "user.name", "Release Test"])
            .status
            .success()
    );
    assert!(
        run_git(
            repo,
            &["config", "user.email", "release-test@example.invalid"]
        )
        .status
        .success()
    );
    assert!(run_git(repo, &["add", "."]).status.success());
    assert!(run_git(repo, &["commit", "-m", "fixture"]).status.success());

    let original_path = std::env::var("PATH").expect("PATH");
    let output = Command::new(&release)
        .arg("2.1.0")
        .current_dir(repo)
        .env("PATH", format!("{}:{original_path}", fake_bin.display()))
        .output()
        .expect("run release script");
    assert!(
        output.status.success(),
        "release failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let changelog = fs::read_to_string(repo.join("CHANGELOG.md")).expect("read changelog");
    assert!(changelog.contains("## Unreleased\n\n## 2.1.0 - "));
    assert!(changelog.contains("## 2.1.0 - 20"));
    assert_eq!(changelog.matches("- Corrected release notes.").count(), 1);
    assert!(changelog.find("## 2.1.0").unwrap() < changelog.find("### Fixed").unwrap());
    assert!(changelog.find("### Fixed").unwrap() < changelog.find("## 2.0.0").unwrap());

    let tag = run_git(repo, &["tag", "-n99", "v2.1.0"]);
    assert!(tag.status.success());
    assert!(
        String::from_utf8_lossy(&tag.stdout).contains("Corrected release notes."),
        "tag message did not include the changelog entry: {}",
        String::from_utf8_lossy(&tag.stdout)
    );
}
