# Agent Memory Landscape and Engram R&D Priorities

Date: 2026-07-28

Status: research note, not an implementation plan

## Executive conclusion

Engram is not merely inspired by `akitaonrails/ai-memory`. It is an
independent hard fork of `ai-memory` at v1.8.0. The public Engram repository
started from a squashed release commit, so GitHub does not expose shared
commit ancestry, but the code and data lineage is explicit in `README.md`,
`CHANGELOG.md`, `LICENSE`, and the migration guide.

The direct and conceptual lineage is:

1. `akitaonrails/ai-memory` v1.8.0: direct code base.
2. Karpathy's LLM Wiki: compile knowledge into a maintained wiki instead of
   rediscovering it from raw chunks on every query.
3. `rohitg00/agentmemory`: practical coding-agent memory ideas; `ai-memory`
   was written as a Rust replacement after operational problems with it.
4. `basic-memory`: Markdown on disk as the human-readable source of truth.
5. `cognee`: composable processing pipelines and graph/triplet retrieval
   ideas.
6. Hermes Agent: post-session learning review, approval gates, and curator
   boundaries.
7. A-MEM: Zettelkasten-like atomic notes and evolving links.

The highest-fit next research direction is not a wholesale graph database or
another vector backend. It is a small, derived, provenance-first temporal
claim layer that keeps Markdown and raw observations as evidence. Before that
architecture bet, Engram needs a real replay/evaluation corpus and optimistic
concurrency for ordinary page writes. Retrieval safety, bounded active
reconstruction, and learned memory control are promising follow-on
experiments, but should begin in shadow or client-side modes.

## Provenance evidence

The repository states that Engram is an independent hard fork of
`akitaonrails/ai-memory` at v1.8.0:

- `README.md:12-20`
- `CHANGELOG.md:5-9`
- `LICENSE:3-4`

The migration guide says the wiki and SQLite formats are identical at the
fork boundary, and the MCP `memory_*` tool names and port remain unchanged:

- `docs/migrate-from-ai-memory.md:3-7`
- `docs/migrate-from-ai-memory.md:13-23`

The upstream v1.8.0 tag resolves to commit:

```text
8c7a82b8d1a201bf4959dce38d7e4f0e981c456e
```

Engram's first public commit is intentionally squashed and records the same
fork point in its commit message. Therefore:

- "hard fork" accurately describes the code and product lineage;
- "GitHub fork with shared visible ancestry" does not describe the current
  public repository history.

Engram has since added or deepened fork-local capabilities, including:

- the desktop workbench and pending-write approval UI;
- Chinese full-text search with a CJK trigram/LIKE routing path;
- long-document chunked embeddings;
- an Obsidian importer;
- further approval, scope, and operational hardening.

Some later changes were selected upstream ports. A feature should not be
called Engram-original merely because it landed after the v1.8.0 fork.

## Conceptual lineage

### Karpathy LLM Wiki

Karpathy's LLM Wiki describes a persistent, compounding Markdown artifact
between raw sources and the model. The LLM integrates new evidence into
existing pages, maintains links, and records contradictions. This is the
origin of the "compile, do not repeatedly rediscover" pattern.

Primary source:

- https://gist.github.com/karpathy/442a6bf555914893e9891c11519de94f

### agentmemory and ai-memory

Fabio Akita first used `agentmemory`, then documented operational failures and
started `ai-memory` as a Rust replacement. Engram inherits this lineage
through its direct fork of `ai-memory`, not through a separate fork of
`agentmemory`.

Primary sources:

- https://akitaonrails.com/en/2026/05/18/ai-agent-memory-karpathy-llm-wiki-agentmemory/
- https://github.com/rohitg00/agentmemory
- https://github.com/akitaonrails/ai-memory/tree/v1.8.0

### Other declared influences

Engram's own README identifies the following additional prior art:

- `basic-memory`: https://github.com/basicmachines-co/basic-memory
- `cognee`: https://github.com/topoteretes/cognee
- Hermes Agent: https://github.com/NousResearch/hermes-agent
- A-MEM: https://arxiv.org/abs/2502.12110

These are design influences, not direct code-fork claims.

## Current Engram position

Engram already has more than a flat RAG store:

- lifecycle-hook capture and typed cross-agent handoffs;
- a Git-versioned Markdown wiki as source of truth;
- SQLite as a derived index;
- page supersession and memory tiers;
- FTS5, link-neighbour, and optional vector retrieval combined with RRF;
- bounded raw-observation fallback;
- decay, lint, consolidation, auto-improvement, and review-gated proposals.

The important remaining gaps are:

1. **Evaluation is synthetic.** The current recall harness uses ten
   hand-written pages and a deterministic non-semantic embedder. The
   architecture document already lists real LongMemEval-S integration as
   future work.
2. **Links are not temporal facts.** The `links` table represents Markdown
   references. It does not encode atomic claims, evidence, confidence, or
   valid-time history.
3. **Ordinary writes have no expected-version precondition.** The current MCP,
   admin, and wiki write request shapes do not carry an expected page version
   or content hash. Parallel agents can therefore overwrite the same logical
   page without a typed stale-write conflict.
4. **Procedural memory is not a mature lifecycle.** The tier exists, but
   extraction, outcome attribution, and reinforcement are not yet developed
   enough to drive a learned controller.

## Evidence-quality warning

Novel memory algorithms are not automatically better systems.

A June 2026 systems characterization evaluated ten memory systems and found,
in one controlled LongMemEval configuration, that BM25 reached 47.0 accuracy,
ahead of GraphRAG at 46.0, HippoRAG v2 at 44.3, A-MEM at 42.7, and several
more expensive agentic systems. The study also found large differences in
construction calls, energy, latency, and storage cost.

Primary source:

- https://arxiv.org/abs/2606.06448
- https://arxiv.org/html/2606.06448v1

This result is not a universal ranking. It is evidence that Engram should
measure construction, retrieval, freshness, and false-memory cost alongside
answer accuracy before adopting a more complex architecture.

## High-novelty candidates

### 1. Temporal, provenance-first claims

**Sources**

- Zep/Graphiti: https://arxiv.org/abs/2501.13956
- Graphiti implementation: https://github.com/getzep/graphiti
- Hindsight: https://aclanthology.org/2026.acl-demo.27/
- Hindsight implementation: https://github.com/vectorize-io/hindsight
- MOSAIC: https://arxiv.org/abs/2607.16211

**Novel mechanism**

Graphiti gives facts validity windows and traces them to source episodes.
Hindsight separates world facts, agent experiences, synthesized
observations, and opinions/beliefs. MOSAIC explores detecting conflicts
against nearby memories at write time rather than globally rebuilding the
store.

**Fit for Engram**

Add a derived SQLite assertion index instead of importing Graphiti's external
graph stack:

```text
claim_id
subject
predicate
object
epistemic_kind
valid_from
valid_to
observed_at
source_page_id
source_observation_id
confidence
status
```

`valid_from`/`valid_to` represent event time. `observed_at` and page-version
history represent transaction time. A correction closes an old validity
interval instead of erasing evidence.

The first implementation should be shadow-only:

- extract claims from existing Markdown/observations;
- never rewrite canonical pages automatically;
- return every claim with an evidence path;
- stage possible conflicts in `pending-writes`;
- keep Markdown and observations as the source of truth.

This is now more plausible than when the prior-art note deferred temporal
triples: basic link extraction and graph-neighbour RRF have since shipped.

**Maturity**

Graphiti has a paper and active implementation. Hindsight has an ACL 2026
system demonstration and an active implementation, although benchmark
numbers remain substantially author-reported. MOSAIC is a very recent
preprint and should be treated as a write-time conflict idea, not a validated
component.

**Do not copy**

- an external Neo4j/FalkorDB dependency;
- multiple authoritative stores;
- automatic fact deletion based on one LLM extraction;
- a compulsory LLM path for zero-LLM users.

### 2. Retrieval-to-prompt trust gating

**Source**

- MemGate: https://arxiv.org/abs/2606.06054

**Novel mechanism**

MemGate treats memory retrieval as a trust boundary. A query-conditioned
small model filters semantically relevant but contextually unsafe memories
before they enter the agent prompt. The threat model includes cross-domain
leakage, sycophancy, tool-call drift, and memory-induced jailbreaks.

**Fit for Engram**

Place a shadow gate between RRF candidates and MCP/handoff serialization.
Begin with a deterministic policy and an evaluation set before considering a
learned model:

- reject scope mismatches;
- penalize untrusted tool-output provenance;
- distinguish user assertions from model-generated inferences;
- flag instructions embedded inside recalled evidence;
- record allow/deny scores without changing results.

This is especially relevant for coding agents because repository files and
tool output can become durable control-channel content.

**Maturity**

MemGate is a June 2026 preprint. Its reported 9M-parameter gate is
interesting, but local calibration and false-positive testing are required.

### 3. Bounded active memory reconstruction

**Sources**

- MRAgent: https://arxiv.org/abs/2606.06036
- Implementation: https://github.com/Ji-shuo/MRAgent

**Novel mechanism**

MRAgent uses a Cue-Tag-Content associative graph. Retrieval is an iterative
process that explores and prunes paths as evidence accumulates, instead of a
single fixed top-k lookup.

**Fit for Engram**

Keep the daemon's existing RRF as the seed retriever, then test an optional
client/orchestrator strategy that:

- runs at most two or three refinement rounds;
- has fixed token, latency, and node-expansion budgets;
- reads only existing evidence nodes;
- returns the traversed evidence path;
- performs no writes during reconstruction.

Only move this into the server if a frozen-corpus evaluation shows a
material multi-hop gain under equal model and token budgets.

**Maturity**

The paper is recent and mainly demonstrates long-horizon QA. It does not yet
establish reliability for concurrent coding-agent memory.

### 4. Learned adaptive memory control

**Sources**

- MemCon: https://arxiv.org/abs/2607.13591
- Implementation: https://github.com/ericjiang18/MemCon

**Novel mechanism**

MemCon models memory operations as a controlled process. A lightweight
contextual bandit chooses when and how much to retrieve, when to re-query,
when to inject a prior plan, and when to consolidate or forget.

**Fit for Engram**

Engram already exposes the candidate actions, but does not yet have a
trustworthy task-success signal. A safe first experiment would therefore:

- log what a controller would have selected;
- leave actual retrieval and maintenance unchanged;
- require explicit task outcome events;
- prohibit autonomous hard deletion;
- evaluate multiple seeds and distribution drift.

**Maturity**

MemCon is a July 2026 preprint. Its backend-agnostic claim and reported gains
need independent reproduction. It should remain behind temporal claims and
trust gating in Engram's roadmap.

### Conditional watchlist

- MemForest: temporal trees and dirty-path refresh may matter only after
  measured scale or refresh bottlenecks appear.
  https://arxiv.org/abs/2605.23986
- MemOS: portable/composable memory bundles are interesting, but a broad
  memory-OS abstraction conflicts with Engram's single Rust binary, SQLite,
  narrow MCP surface, and Markdown source of truth.
  https://arxiv.org/abs/2507.03724
- Mem0's evolving append-only extraction can serve as a simpler baseline for
  whether consolidation is earning its cost.
  https://github.com/mem0ai/mem0

## Recommended order

### P0: Build a real replay/evaluation gate

Extend the existing recall harness rather than creating a separate benchmark
framework. Include LongMemEval-S/MemoryAgentBench-compatible data plus
coding-agent probes for:

- exact fact recovery;
- current versus historical values;
- delayed corrections and contradictions;
- similar but non-conflicting claims;
- multi-hop dependency recall;
- negative retrieval and selective forgetting;
- source/provenance exactness;
- cross-project isolation;
- poisoned-memory and tool-drift resistance;
- procedural-plan reuse.

Record:

- recall@k and answer accuracy;
- temporal and conflict precision/recall;
- provenance exactness;
- false-memory rate;
- p50/p95 construction and query latency;
- LLM/embedding calls and tokens;
- index/storage growth;
- freshness lag.

### P1: Add optimistic concurrency to ordinary writes

Before more autonomous rewriting, let MCP/admin writers supply an expected
page hash or version. A stale writer should receive a typed conflict instead
of silently superseding a newer page.

This addresses Engram's actual multi-agent use case and protects every later
claim or consolidation experiment.

### P2: Add a shadow temporal assertion index

Implement the minimal derived claim schema, source every assertion, and
evaluate current-value, historical, correction, and provenance queries.
Do not alter canonical Markdown automatically.

### P3: Test trust gating and staged conflict detection

Run both in shadow mode. Conflict detectors may propose a review item; they
must not update or delete memories autonomously. Promote only after high
precision on project-specific adversarial and contradiction datasets.

### P4: Compare bounded reconstruction against fixed RRF

Keep it client-side initially. Use the same corpus, model, token budget, and
answer judge. Require evidence paths and hard iteration limits.

### P5: Consider adaptive control only after outcome instrumentation

MemCon-like control is not meaningful until Engram can distinguish task
success from a completed session and can evaluate policies over repeated
task families.

## Architecture constraints to preserve

- Markdown and raw observations remain evidence/source of truth.
- New graph, claim, or safety structures are derived and rebuildable.
- SQLite remains the default storage substrate.
- Zero-LLM operation remains functional.
- No autonomous irreversible deletes.
- Every synthesized claim has provenance and version history.
- Agentic read loops have explicit hop, token, latency, and call limits.
- New capability does not automatically imply a new MCP tool.
- Benchmarks include operational cost and security, not only QA accuracy.

## Bottom line

Engram's lineage is already well documented: direct hard fork from
`ai-memory`, with a wider family of declared design influences. The most
defensible next novelty is a provenance-first temporal/epistemic claim layer,
not a replacement storage stack. The responsible sequence is:

```text
real evaluation
  -> concurrent-write safety
  -> shadow temporal claims
  -> trust/conflict gates
  -> bounded active reconstruction
  -> learned adaptive control
```

That sequence makes each architecture step falsifiable and preserves Engram's
current strengths: inspectability, portability, narrow interfaces, and
recoverable human review.
