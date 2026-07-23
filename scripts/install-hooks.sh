#!/usr/bin/env bash
# Curl-based installer for engram's lifecycle-hook scripts.
#
# Use when you don't want to clone the repo or unpack the release
# archive to get the bundle. Pulls each hook script for the
# requested agent straight from the published GitHub raw URL.
#
# Usage:
#   curl -sSL https://raw.githubusercontent.com/semantic-craft/engram/main/scripts/install-hooks.sh \
#       | bash -s -- --agent claude-code
#
# Options:
#   --agent <claude-code|codex|cursor|gemini-cli|antigravity-cli|grok|opencode|openclaw|omp|oh-my-pi|pi>
#                                                which agent (default: claude-code;
#                                                generated-plugin agents print hints)
#   --to <dir>                               install root (default: $HOME/.engram/hooks)
#   --ref <git-ref>                          repo ref to pull from (default: main)
#
# After installation, render the matching agent config snippet with the
# engram binary from the release archive:
#   engram install-hooks --agent claude-code --hooks-dir ~/.engram/hooks

set -euo pipefail

AGENT="claude-code"
TO="$HOME/.engram/hooks"
REF="main"
REPO="semantic-craft/engram"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --agent)  AGENT="$2"; shift 2 ;;
        --to)     TO="$2"; shift 2 ;;
        --ref)    REF="$2"; shift 2 ;;
        --repo)   REPO="$2"; shift 2 ;;
        -h|--help)
            sed -n '2,18p' "$0"
            exit 0 ;;
        *)
            echo "unknown flag: $1" >&2
            exit 64 ;;
    esac
done

case "$AGENT" in
    claude-code|codex|cursor|gemini-cli|antigravity-cli|grok|opencode|openclaw|omp|pi|oh-my-pi) ;;
    *)
        echo "unsupported agent: $AGENT (expected claude-code | codex | cursor | gemini-cli | antigravity-cli | grok | opencode | openclaw | omp | pi | oh-my-pi)" >&2
        exit 64 ;;
esac

if [[ "$AGENT" == "opencode" ]]; then
    echo "OpenCode uses a generated TypeScript plugin, not shell hook scripts."
    echo "Run: engram install-hooks --agent opencode --apply"
    echo "Then restart OpenCode so it loads ~/.config/opencode/plugins/engram.ts."
    exit 0
fi

if [[ "$AGENT" == "openclaw" ]]; then
    echo "OpenClaw uses a generated native TypeScript plugin, not shell hook scripts."
    echo "Run: engram install-hooks --agent openclaw --apply"
    echo "Then restart the OpenClaw gateway if it does not auto-restart after plugin install."
    exit 0
fi

if [[ "$AGENT" == "omp" || "$AGENT" == "oh-my-pi" ]]; then
    echo "OMP uses a generated TypeScript extension, not shell hook scripts."
    echo "Run: engram install-hooks --agent omp --apply"
    echo "Then restart OMP so it loads ~/.omp/agent/extensions/engram.ts."
    exit 0
fi

if [[ "$AGENT" == "pi" ]]; then
    echo "Pi uses a generated TypeScript extension, not shell hook scripts."
    echo "Run: engram install-hooks --agent pi --apply"
    echo "Then restart Pi so it loads ~/.pi/agent/extensions/engram.ts."
    echo "MCP tools come through the same generated bridge extension."
    exit 0
fi

SCRIPTS=(
    "session-start"
    "user-prompt-submit"
    "pre-tool-use"
    "post-tool-use"
    "pre-compact"
    "stop"
    "session-end"
)

DEST="$TO/$AGENT"
mkdir -p "$DEST"

echo "Installing engram hooks for $AGENT into $DEST"
for name in "${SCRIPTS[@]}"; do
    url="https://raw.githubusercontent.com/$REPO/$REF/hooks/$AGENT/${name}.sh"
    out="$DEST/${name}.sh"
    if curl -fsSL "$url" -o "$out"; then
        chmod +x "$out"
        echo "  ✓ $name"
    else
        echo "  ✗ $name (failed to fetch $url)" >&2
        exit 1
    fi
done

echo
echo "Done. Next steps:"
echo
echo "  1. Render the config snippet to merge into your agent's settings:"
echo "       engram install-hooks --agent $AGENT --hooks-dir $TO"
echo
echo "  2. If your server uses bearer-token auth, pass --auth-token <token>"
echo "     to the install-hooks command so the snippet wires it in."
