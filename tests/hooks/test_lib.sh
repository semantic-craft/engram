#!/bin/sh
# Smoke tests for hooks/_lib.sh. Run from the repo root:
#
#   sh tests/hooks/test_lib.sh
#
# Exits non-zero on any failure. POSIX shell + sed/awk only, so no extra
# CI setup needed.
set -eu

# shellcheck source=../../hooks/_lib.sh
. "$(dirname "$0")/../../hooks/_lib.sh"

PASS=0
FAIL=0
TMP=$(mktemp -d)
# Pin HOME inside the temp tree so walk-up never leaves the sandbox.
ORIG_HOME=${HOME:-}
HOME="$TMP"
export HOME
trap 'rm -rf "$TMP"; HOME=$ORIG_HOME' EXIT

assert_eq() {
    desc="$1"; want="$2"; got="$3"
    if [ "$want" = "$got" ]; then
        PASS=$((PASS+1))
        printf '  ok  %s\n' "$desc"
    else
        FAIL=$((FAIL+1))
        printf '  FAIL %s\n    want=%s\n    got =%s\n' "$desc" "$want" "$got"
    fi
}

# --- parse_toml_key ---------------------------------------------------
cat >"$TMP/sample.toml" <<EOF
# Comment line
workspace = "movvia"
project = "pe-portais"
project_strategy = "repo-root"

# Trailing comment
EOF

assert_eq "parse workspace"           "movvia"     "$(engram_parse_toml_key "$TMP/sample.toml" workspace)"
assert_eq "parse project"             "pe-portais" "$(engram_parse_toml_key "$TMP/sample.toml" project)"
assert_eq "parse project_strategy"    "repo-root"  "$(engram_parse_toml_key "$TMP/sample.toml" project_strategy)"
assert_eq "absent key returns empty"  ""           "$(engram_parse_toml_key "$TMP/sample.toml" missing)"
assert_eq "absent file returns empty" ""           "$(engram_parse_toml_key "$TMP/no-such-file.toml" workspace)"

# --- find_marker ------------------------------------------------------
mkdir -p "$TMP/a/b/c/d"
printf 'workspace = "deep"\n' >"$TMP/a/.engram.toml"
assert_eq "walks up to find marker" "$TMP/a/.engram.toml" \
    "$(engram_find_marker "$TMP/a/b/c/d")"
assert_eq "no marker returns empty" "" \
    "$(engram_find_marker "$TMP/nonexistent/path")"

# --- extract_cwd ------------------------------------------------------
PAYLOAD='{"session_id":"x","cwd":"/home/u/foo","tool":"Read"}'
assert_eq "extract cwd from payload"     "/home/u/foo" "$(engram_extract_cwd "$PAYLOAD")"
assert_eq "extract cwd from empty json"  ""            "$(engram_extract_cwd '{}')"
PAYLOAD_NESTED='{"session_id":"x","cwd":"/home/u/root","tool_input":{"cwd":"/tmp/nested"}}'
assert_eq "extract cwd prefers first match" "/home/u/root" "$(engram_extract_cwd "$PAYLOAD_NESTED")"
PAYLOAD_AGY='{"conversationId":"x","workspacePaths":["/home/u/agy","/tmp/other"]}'
assert_eq "extract cwd from antigravity workspacePaths" "/home/u/agy" "$(engram_extract_cwd "$PAYLOAD_AGY")"

# --- json_string -------------------------------------------------------
JSON_INPUT='quoted "thing" \ path
next line'
assert_eq "json_string escapes text" '"quoted \"thing\" \\ path\nnext line"' \
    "$(printf '%s' "$JSON_INPUT" | engram_json_string)"

# --- marker_qs --------------------------------------------------------
QS=$(engram_marker_qs "$TMP/a/b/c")
assert_eq "marker_qs single key" "&cwd=$TMP/a/b/c&workspace=deep" "$QS"

printf 'workspace = "ws1"\nproject = "p1"\nproject_strategy = "repo-root"\n' >"$TMP/a/b/.engram.toml"
QS2=$(engram_marker_qs "$TMP/a/b/c")
assert_eq "closer marker wins" "&cwd=$TMP/a/b/c&workspace=ws1&project=p1&project_strategy=repo-root" "$QS2"

QS3=$(engram_marker_qs "$TMP/nonexistent")
assert_eq "no marker -> cwd only" "&cwd=$TMP/nonexistent" "$QS3"

# --- repo-root strategy: host-side resolution -------------------------
# Outside any git repo the helper stays silent (caller keeps basename(cwd)).
assert_eq "repo_root_project on non-git path is empty" "" \
    "$(engram_repo_root_project "$TMP/nonexistent")"

if command -v git >/dev/null 2>&1; then
    REPO="$TMP/repos/acme-api"
    mkdir -p "$REPO"
    git init -q "$REPO"
    git -C "$REPO" -c user.email=t@example.com -c user.name=t \
        commit -q --allow-empty -m init

    # A subdirectory of the main checkout collapses to the repo basename
    # (not the subdir name) when the marker selects repo-root and pins no
    # explicit project.
    mkdir -p "$REPO/crates/cli"
    printf 'workspace = "oss"\nproject_strategy = "repo-root"\n' >"$REPO/.engram.toml"
    QSR=$(engram_marker_qs "$REPO/crates/cli")
    assert_eq "repo-root: subdir resolves to repo basename" \
        "&cwd=$REPO/crates/cli&workspace=oss&project=acme-api&project_strategy=repo-root" \
        "$QSR"

    rm -f "$REPO/.engram.toml"
    ENGRAM_PROJECT_STRATEGY=repo-root
    export ENGRAM_PROJECT_STRATEGY
    QSE=$(engram_marker_qs "$REPO/crates/cli")
    assert_eq "repo-root env: no marker resolves to repo basename" \
        "&cwd=$REPO/crates/cli&project=acme-api&project_strategy=repo-root" \
        "$QSE"

    printf 'workspace = "oss"\nproject = "pinned"\nproject_strategy = "basename"\n' \
        >"$REPO/.engram.toml"
    QSO=$(engram_marker_qs "$REPO/crates/cli")
    assert_eq "marker project strategy overrides env default" \
        "&cwd=$REPO/crates/cli&workspace=oss&project=pinned&project_strategy=basename" \
        "$QSO"
    unset ENGRAM_PROJECT_STRATEGY

    printf 'workspace = "oss"\nproject_strategy = "repo-root"\n' >"$REPO/.engram.toml"

    # A linked worktree whose directory lives OUTSIDE the main repo tree
    # (a common layout: tools that keep worktrees in a separate directory)
    # has no .engram.toml ancestor of its own, yet still collapses to the
    # MAIN repo basename via the commondir pointer. The strategy comes from a
    # marker placed above the worktrees directory.
    WT="$TMP/worktrees/acme-api/wt-feature"
    mkdir -p "$TMP/worktrees/acme-api"
    printf 'workspace = "oss"\nproject_strategy = "repo-root"\n' >"$TMP/worktrees/.engram.toml"
    if git -C "$REPO" worktree add -q "$WT" >/dev/null 2>&1; then
        QSW=$(engram_marker_qs "$WT")
        assert_eq "repo-root: out-of-tree worktree collapses to main repo" \
            "&cwd=$WT&workspace=oss&project=acme-api&project_strategy=repo-root" \
            "$QSW"
    fi

    # An explicit project pin always wins over repo-root resolution.
    printf 'workspace = "oss"\nproject = "pinned"\nproject_strategy = "repo-root"\n' \
        >"$REPO/.engram.toml"
    QSP=$(engram_marker_qs "$REPO/crates/cli")
    assert_eq "explicit project pin beats repo-root" \
        "&cwd=$REPO/crates/cli&workspace=oss&project=pinned&project_strategy=repo-root" \
        "$QSP"

    PSH=""
    if command -v pwsh >/dev/null 2>&1; then
        PSH=$(command -v pwsh)
    elif command -v powershell >/dev/null 2>&1; then
        PSH=$(command -v powershell)
    fi
    if [ -n "$PSH" ]; then
        PS_REPO=$($PSH -NoProfile -ExecutionPolicy Bypass -Command \
            ". '$PWD/hooks/lib/engram-hook.ps1'; Get-EngramRepoRootProject -Cwd '$REPO/crates/cli'")
        assert_eq "powershell repo-root helper resolves repo basename" "acme-api" "$PS_REPO"
    else
        PS_STATIC=$(grep -q 'function Get-EngramRepoRootProject' hooks/lib/engram-hook.ps1 \
            && grep -q -- '--git-common-dir' hooks/lib/engram-hook.ps1 \
            && grep -q 'Get-EngramRepoRootProject -Cwd' hooks/lib/engram-hook.ps1 \
            && printf 'ok' || printf 'missing')
        assert_eq "powershell repo-root helper has static parity" "ok" "$PS_STATIC"
    fi
fi

# --- url_encode -------------------------------------------------------
assert_eq "url_encode passes safe slug"   "movvia" "$(engram_url_encode "movvia")"
assert_eq "url_encode escapes ampersand"  "a%26b"  "$(engram_url_encode "a&b")"
assert_eq "url_encode escapes equals"     "a%3Db"  "$(engram_url_encode "a=b")"
assert_eq "url_encode escapes plus"       "a%2Bb"  "$(engram_url_encode "a+b")"

# --- summary ----------------------------------------------------------
printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
