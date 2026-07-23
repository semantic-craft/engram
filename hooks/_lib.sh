# engram hook helper — find marker file + parse minimal TOML.
# Sourced by per-agent lifecycle hook scripts. POSIX shell only —
# no bash-isms, no non-standard deps (no jq, no toml crate). Keep changes
# byte-trivial because every supported agent (claude-code, codex,
# cursor, gemini-cli, antigravity-cli, opencode, omp) sources this same file.

# Walk up from "$1" toward $HOME (or /) looking for `.engram.toml`.
# Prints the absolute path of the first marker found, or nothing.
# Stops at $HOME to avoid leaking declarations from a shared system
# user's home into another user's session on multi-user boxes.
engram_find_marker() {
    dir="$1"
    [ -z "$dir" ] && return 0
    while [ -n "$dir" ] && [ "$dir" != "/" ]; do
        if [ -f "$dir/.engram.toml" ]; then
            printf '%s\n' "$dir/.engram.toml"
            return 0
        fi
        if [ -n "${HOME:-}" ] && [ "$dir" = "$HOME" ]; then
            return 0
        fi
        parent=$(dirname "$dir")
        [ "$parent" = "$dir" ] && return 0
        dir="$parent"
    done
}

# Parse `key = "value"` at the TOML root (no nesting, no arrays, no
# tables). Returns the first match or nothing. Ignores comments and
# blank lines by construction (the regex only matches the `key = "..."`
# shape).
engram_parse_toml_key() {
    file="$1"; key="$2"
    [ -f "$file" ] || return 0
    sed -n -E "s/^[[:space:]]*${key}[[:space:]]*=[[:space:]]*\"([^\"]*)\".*/\1/p" \
        "$file" | head -n 1
}

# Extract the first cwd-like path from a JSON payload on stdin or in $1.
# Returns the value or nothing. This is intentionally a tiny shell fallback,
# not a JSON parser; taking the first match preserves the top-level cwd when
# tool payloads contain nested `cwd` fields later in the object. Antigravity
# CLI sends `workspacePaths: ["/repo", ...]` instead of `cwd`.
# Undo the JSON string escapes that can appear in a path value: \\ -> \
# and \/ -> /. Windows payloads carry cwd as "C:\\dev\\proj"; without this
# the doubled backslashes leak into the query string (#188).
engram_json_unescape_path() {
    printf '%s' "$1" | sed 's/\\\\/\\/g; s/\\\//\//g'
}

engram_extract_cwd() {
    payload="${1:-$(cat)}"
    rest=${payload#*\"cwd\"}
    if [ "$rest" != "$payload" ]; then
        raw=$(printf '%s' "$rest" \
            | sed -n -E 's/^[[:space:]]*:[[:space:]]*"([^"]*)".*/\1/p' \
            | head -n 1)
        engram_json_unescape_path "$raw"
        return 0
    fi
    rest=${payload#*\"workspacePaths\"}
    [ "$rest" = "$payload" ] && return 0
    raw=$(printf '%s' "$rest" \
        | sed -n -E 's/^[[:space:]]*:[[:space:]]*\[[[:space:]]*"([^"]*)".*/\1/p' \
        | head -n 1)
    engram_json_unescape_path "$raw"
}

# URL-encode the minimal set of characters that have meaning in a query
# string. Sufficient for the schema's value regex (`^[a-z0-9][a-z0-9._-]*$`)
# plus a defensive pass for anything a hand-edited marker might contain.
# Percent-encode everything outside the RFC 3986 unreserved set
# (A-Z a-z 0-9 - _ . ~), byte-wise under LC_ALL=C so multibyte UTF-8 is
# encoded per byte. Allow-list on purpose: the old deny-list missed
# backslash, so a Windows cwd went into the query string raw and the
# request never reached the server (#188). Parity with the native
# helper's url_encode in hook_capture.rs.
engram_url_encode() {
    LC_ALL=C
    s="$1"
    out=""
    while [ -n "$s" ]; do
        rest="${s#?}"
        c="${s%"$rest"}"
        s="$rest"
        case $c in
            [A-Za-z0-9._~-]) out="$out$c" ;;
            *) out="$out$(printf '%%%02X' "'$c")" ;;
        esac
    done
    printf '%s' "$out"
}

# Resolve the basename of the MAIN git repository root for "$1" (a cwd),
# following the worktree commondir pointer so every linked worktree of a
# repo collapses to one stable name. Mirrors the server's
# `discover_main_repo_root` (libgit2) but runs host-side, where the
# checkout is always visible — the server cannot do this when it runs in a
# container that has no access to the host filesystem (its own discovery
# fails and falls back to basename(cwd), so out-of-tree worktrees each
# became their own project). Prints the name, or nothing when cwd is not
# inside a git work tree (caller keeps its basename(cwd) fallback).
engram_repo_root_project() {
    cwd="$1"
    [ -z "$cwd" ] && return 0
    command -v git >/dev/null 2>&1 || return 0
    # Only touch git when cwd is genuinely inside a working tree. Outside any
    # repo, or inside a bare repo, `--is-inside-work-tree` is not "true" and
    # we stay silent rather than guess.
    [ "$(git -C "$cwd" rev-parse --is-inside-work-tree 2>/dev/null)" = "true" ] || return 0
    # `--git-common-dir` is the shared `.git` dir: for a worktree it points
    # at the MAIN repo's `.git`, so its parent is always the main repo root.
    common=$(git -C "$cwd" rev-parse --path-format=absolute --git-common-dir 2>/dev/null) || return 0
    [ -n "$common" ] || return 0
    root=$(dirname "$common")
    case "$root" in
        "" | /) return 0 ;;
    esac
    basename "$root"
}

# Build a query-string suffix from "$1" plus any marker file walked up from
# it. Returns the suffix with the leading `&`, or nothing when cwd is absent.
# `cwd` is always included so `GET /handoff` resolves the same basename project
# as the prior hook events even when no marker file exists.
engram_marker_qs() {
    cwd="$1"
    [ -z "$cwd" ] && return 0
    qs="&cwd=$(engram_url_encode "$cwd")"
    ws=""
    pr=""
    st=""
    ds=""
    marker=$(engram_find_marker "$cwd")
    if [ -n "$marker" ]; then
        ws=$(engram_parse_toml_key "$marker" workspace)
        pr=$(engram_parse_toml_key "$marker" project)
        st=$(engram_parse_toml_key "$marker" project_strategy)
        ds=$(engram_parse_toml_key "$marker" drop_subagent_captures)
    fi
    # Install-time default baked into the hook command by
    # `install-hooks --project-strategy` fills the strategy only when no marker
    # pinned one. A marker's explicit project / project_strategy still win.
    if [ -z "$st" ] && [ -n "${ENGRAM_PROJECT_STRATEGY:-}" ]; then
        st="$ENGRAM_PROJECT_STRATEGY"
    fi
    # The repo-root strategy must be resolved here, on the host: a containerized
    # server cannot see this checkout, so its own libgit2 discovery fails and
    # falls back to basename(cwd). When repo-root is selected and no explicit
    # project is pinned, derive the main repo name now and send it as an explicit
    # `project` override. `project_strategy` is still forwarded so native servers
    # keep their existing resolution path.
    if [ -z "$pr" ]; then
        case "$st" in
            repo-root | repo_root) pr=$(engram_repo_root_project "$cwd") ;;
        esac
    fi
    [ -n "$ws" ] && qs="${qs}&workspace=$(engram_url_encode "$ws")"
    [ -n "$pr" ] && qs="${qs}&project=$(engram_url_encode "$pr")"
    [ -n "$st" ] && qs="${qs}&project_strategy=$(engram_url_encode "$st")"
    # Per-project drop_subagent_captures opt-in: forward to the server, which
    # interprets truthiness (1/true/...) and scopes the drop to this project.
    [ -n "$ds" ] && qs="${qs}&drop_subagent=$(engram_url_encode "$ds")"
    printf '%s' "$qs"
}

# POST stdin to "$1" as JSON, fire-and-forget. Adds an
# `Authorization: Bearer` header when `ENGRAM_AUTH_TOKEN` is set.
# The 0.5s timeout matches the project-wide hook latency budget
# (never block the agent), and the trailing `|| true` makes the
# function safe to call from `set -e` scripts.
engram_post_hook() {
    if [ -n "${ENGRAM_AUTH_TOKEN:-}" ]; then
        curl -s --max-time 0.5 -X POST "$1" \
            -H "Content-Type: application/json" \
            -H "Authorization: Bearer $ENGRAM_AUTH_TOKEN" \
            --data-binary @-
    else
        curl -s --max-time 0.5 -X POST "$1" \
            -H "Content-Type: application/json" \
            --data-binary @-
    fi
}

# GET "$1" with the same auth-header rules as `engram_post_hook`.
# Used by `session-start.sh` to pull the cross-agent handoff before
# the resuming agent's first prompt. 1s budget — slightly more
# generous than POST because the result is *synchronously* fed to
# stdout (and prepended to the agent's context), so we want to avoid
# truncating a handoff that was almost ready.
engram_get_handoff() {
    if [ -n "${ENGRAM_AUTH_TOKEN:-}" ]; then
        curl -s --max-time 1.0 "$1" \
            -H "Authorization: Bearer $ENGRAM_AUTH_TOKEN"
    else
        curl -s --max-time 1.0 "$1"
    fi
}

# Encode stdin as a JSON string (with surrounding quotes). Used by hooks
# whose stdout contract is JSON rather than raw context text: Antigravity's
# PreInvocation hook and Claude Code's session-start hook (which wraps the
# handoff in hookSpecificOutput.additionalContext).
engram_json_string() {
    awk '
        BEGIN { printf "\"" }
        {
            gsub(/\\/, "\\\\")
            gsub(/"/, "\\\"")
            gsub(/\t/, "\\t")
            gsub(/\r/, "\\r")
            printf "%s%s", sep, $0
            sep = "\\n"
        }
        END { printf "\"" }
    '
}
