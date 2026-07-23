# `evals/` — live A/B harness

A small Rust binary that runs the EXACT consolidation prompt
engram uses in production against two LLM providers side by side,
and saves both outputs to disk for human comparison.

**This is not part of the shipped binary.** It's a workspace member
purely so it shares deps + builds with the rest. `cargo build` from
the repo root will compile it, but it's never bundled into the
shipped release archive and never run by CI.

## When to reach for this

After switching providers, models, or major prompt edits. Concretely:

- Replacing OpenRouter/Kimi with local Ollama (the case that
  motivated this harness — see commit log).
- Trying a new Ollama model (`qwen3:32b` → `qwen3-coder:30b`).
- Tuning the `BATCH_SYSTEM_PROMPT` itself — does the rewrite
  preserve quality across providers?

The fixtures are deliberately small synthetic session logs that
exercise the prompt's hard cases (durable rule extraction,
multi-topic separation, "say nothing" sessions, decision/gotcha
distinction).

## What it does

For each `*.json` under `evals/fixtures/`:

1. Builds the request via
   [`engram_consolidate::build_batch_request`] — same code path
   the live consolidator uses.
2. Sends it to a **baseline** and a **candidate** provider, *in
   parallel*.
3. Runs the result through
   [`engram_llm::complete_structured`] — same JSON-schema
   validation the live system applies. Schema-parse failure is
   recorded (not fatal).
4. Persists to `evals/runs/<timestamp>/{baseline,candidate}/<fixture>.{json,md,meta.json}`.

The runner prints latency + parse status per fixture and a tail
summary. **Quality is for you to read** — open the markdown files
in `runs/<timestamp>/baseline/` and `…/candidate/` side by side and
judge faithfulness, scoping, hallucination, etc.

## Running it

### Ollama qwen3:32b vs OpenRouter Kimi (the canonical comparison)

```bash
# OPENROUTER_API_KEY in env; LLM_API_KEY can be any non-empty
# string for the candidate (Ollama doesn't validate).
export OPENROUTER_API_KEY="sk-or-v1-..."

cargo run -p engram-eval -- \
    --baseline-provider openai-compat \
    --baseline-base-url https://openrouter.ai/api/v1 \
    --baseline-model moonshotai/kimi-k2.6 \
    --baseline-api-key-env OPENROUTER_API_KEY \
    --candidate-provider openai-compat \
    --candidate-base-url http://192.0.2.10:11434/v1 \
    --candidate-model qwen3:32b \
    --candidate-api-key ollama-local
```

### Two Ollama models against each other

```bash
cargo run -p engram-eval -- \
    --baseline-provider openai-compat \
    --baseline-base-url http://192.0.2.10:11434/v1 \
    --baseline-model qwen3:32b \
    --baseline-api-key ollama-local \
    --candidate-provider openai-compat \
    --candidate-base-url http://192.0.2.10:11434/v1 \
    --candidate-model qwen3-coder:30b \
    --candidate-api-key ollama-local
```

### ChatGPT/Codex OAuth as one side

Run `engram auth login openai-oauth` first, then point the eval harness at
the same token file:

```bash
cargo run -p engram-eval -- \
    --baseline-provider openai-oauth \
    --baseline-token-file ~/.local/share/engram/auth.json \
    --baseline-model gpt-5.5 \
    --candidate-provider openai-compat \
    --candidate-base-url http://192.0.2.10:11434/v1 \
    --candidate-model qwen3:32b \
    --candidate-api-key ollama-local
```

### GitHub Copilot as one side

Run `engram auth login copilot` first, then point the eval harness at the
same auth file:

```bash
cargo run -p engram-eval -- \
    --baseline-provider copilot \
    --baseline-token-file ~/.local/share/engram/auth.json \
    --baseline-model gpt-5.5 \
    --candidate-provider openai-compat \
    --candidate-base-url http://192.0.2.10:11434/v1 \
    --candidate-model qwen3:32b \
    --candidate-api-key ollama-local
```

### Reading the output

```
evals/runs/2026-05-22T18-30-00Z/
├── baseline/
│   ├── 01-rust-bug-fix.json        ← raw structured output
│   ├── 01-rust-bug-fix.md          ← flat markdown rendering, easy to read
│   └── 01-rust-bug-fix.meta.json   ← {elapsed_ms, parsed_ok, update_count}
└── candidate/
    └── …
```

Eyeball the `.md` files in pairs. The runner also prints a
`diff -ru baseline candidate` command you can run.

## Adding fixtures

Each fixture is a JSON file:

```json
{
  "name": "human-readable",
  "description": "what this case is meant to surface",
  "observations": [
    {"kind": "session-start", "title": "...", "body": "..."},
    {"kind": "user-prompt",   "title": "user prompt", "body": "..."},
    {"kind": "pre-tool-use",  "title": "Edit", "body": "..."},
    ...
  ]
}
```

`kind` values: `session-start`, `user-prompt`, `pre-tool-use`,
`post-tool-use`, `pre-compact`, `notification`, `stop`,
`session-end`, `other` (see `ObservationKind` in `engram-core`).

The `description` field isn't read by the runner — it's a comment
for the next human who opens the file.

## What this harness does NOT do

- **Score quality automatically.** Pure side-by-side. If you want
  metrics, the next layer up would be keyword recall (`must_mention`
  per fixture) or an LLM-as-judge pass — both deliberately out of
  scope here.
- **Test the embedding pipeline.** This only exercises the
  consolidation LLM. Embedding A/B would be a parallel harness
  (probe queries + expected target pages, measure recall@5/MRR).
- **Persist via the real wiki layer.** No SQLite, no markdown
  writes, no git. Pure prompt → response.

## Cleanup

`evals/runs/` is in `.gitignore`. Drop it whenever it gets large.
