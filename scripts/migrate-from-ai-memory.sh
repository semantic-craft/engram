#!/usr/bin/env bash
# Migrate a live ai-memory deployment on THIS machine to Engram.
# Dry-run by default; pass --apply to execute. See
# docs/migrate-from-ai-memory.md for the full runbook and rollback.
set -euo pipefail

APPLY=0
[[ "${1:-}" == "--apply" ]] && APPLY=1

OLD_LABEL="com.semantic-craft.ai-memory"
NEW_LABEL="com.semantic-craft.engram"
NEW_BIN="$HOME/.local/bin/engram"

# macOS-only: this migrator drives launchd (launchctl). engram now ships
# only macOS (arm64) and Windows (x86_64) binaries; a Windows ai-memory
# install must be migrated by hand.
OLD_DATA="$HOME/Library/Application Support/ai-memory"
NEW_DATA="$HOME/Library/Application Support/engram"

say()  { printf '%s\n' "$*"; }
run()  { if [[ $APPLY -eq 1 ]]; then say "+ $*"; "$@"; else say "would: $*"; fi }
fail() { say "ERROR: $*" >&2; exit 1; }

say "=== engram migration ($([[ $APPLY -eq 1 ]] && echo APPLY || echo dry-run)) ==="

# --- preflight -------------------------------------------------------
[[ "$(uname -s)" == "Darwin" ]] || fail "this migration helper is macOS-only (uses launchd); migrate a Windows install by hand"
[[ -x "$NEW_BIN" ]] || fail "new binary not found at $NEW_BIN (install it first)"
[[ -d "$OLD_DATA" ]] || fail "old data dir not found: $OLD_DATA (already migrated?)"
[[ -e "$NEW_DATA" ]] && fail "target data dir already exists: $NEW_DATA (refusing to overwrite)"

# --- 1. stop the old service ----------------------------------------
OLD_PLIST="$HOME/Library/LaunchAgents/$OLD_LABEL.plist"
if launchctl print "gui/$(id -u)/$OLD_LABEL" >/dev/null 2>&1; then
  run launchctl bootout "gui/$(id -u)/$OLD_LABEL"
else
  say "old launchd service not loaded (ok)"
fi

# --- 2. move the data dir (mv, not copy — rollback is mv back) ------
run mv "$OLD_DATA" "$NEW_DATA"

# config.toml lives inside the data dir; rewrite self-references
CFG="$NEW_DATA/config.toml"
if [[ -f "$CFG" ]] && grep -q 'ai-memory' "$CFG" 2>/dev/null; then
  run sed -i.pre-engram 's/ai-memory/engram/g' "$CFG"
fi

# --- 3. daemon wrapper: copy + rewrite, retire the old one ----------
OLD_WRAP="$HOME/.local/bin/ai-memory-daemon.sh"
NEW_WRAP="$HOME/.local/bin/engram-daemon.sh"
if [[ -f "$OLD_WRAP" ]]; then
  if [[ $APPLY -eq 1 ]]; then
    sed -e 's/AI_MEMORY_/ENGRAM_/g' -e 's/ai-memory/engram/g' "$OLD_WRAP" >"$NEW_WRAP"
    chmod +x "$NEW_WRAP"
    mv "$OLD_WRAP" "$OLD_WRAP.retired"
    say "+ wrote $NEW_WRAP, retired $OLD_WRAP"
  else
    say "would: derive $NEW_WRAP from $OLD_WRAP (sed rename), retire old wrapper"
  fi
else
  say "note: no wrapper at $OLD_WRAP — daemon may be launched differently; adjust manually"
fi

# --- 4. install + start the new service -----------------------------
NEW_PLIST="$HOME/Library/LaunchAgents/$NEW_LABEL.plist"
if [[ $APPLY -eq 1 ]]; then
  sed -e "s/$OLD_LABEL/$NEW_LABEL/g" -e 's/ai-memory/engram/g' "$OLD_PLIST" >"$NEW_PLIST"
  mv "$OLD_PLIST" "$OLD_PLIST.retired"
  launchctl bootstrap "gui/$(id -u)" "$NEW_PLIST"
  say "+ installed and started $NEW_LABEL"
else
  say "would: derive $NEW_PLIST from $OLD_PLIST, retire old plist, bootstrap service"
fi

# --- 5. health check -------------------------------------------------
if [[ $APPLY -eq 1 ]]; then
  sleep 2
  code=$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:49374/mcp || true)
  [[ "$code" == "405" || "$code" == "200" ]] \
    && say "OK: daemon answering on 127.0.0.1:49374 (HTTP $code)" \
    || fail "daemon not healthy (HTTP $code) — see rollback in docs/migrate-from-ai-memory.md"
fi

say ""
say "Next (manual, see docs/migrate-from-ai-memory.md §3):"
say "  - engram install-hooks --apply   (per agent CLI)"
say "  - update global CLAUDE.md references (ai-memory → engram)"
say "  - per active project: engram install-instructions"
say "  - grep -rn 'AI_MEMORY_\\|ai-memory' ~/.zshrc ~/.config ~/.local/bin"
