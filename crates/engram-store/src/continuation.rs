//! Shared continuation-context assembly.
//!
//! One implementation of "what does a receiver of this Handoff get to read",
//! used by BOTH continuation paths:
//!
//! * the on-demand MCP `memory_handoff_claim` tool, and
//! * automatic SessionStart recovery in the hook router.
//!
//! Keeping it here is the point: adapter-specific code translates lifecycle
//! events and delivery capability only. If the two paths each assembled their
//! own package they would drift on quotas, priority, deduplication, omission
//! reporting, or scope resolution, and "the SessionStart block is the same
//! thing the claim tool returns" would stop being true.
//!
//! Retrieval candidate *generation* still degrades exactly the way
//! `memory_query` does when no embedder is configured: BM25/FTS only, no
//! provider call. Selection, budgeting, and trace semantics are identical
//! either way, so this module works with no LLM provider at all.

use std::collections::{BTreeSet, HashMap, HashSet};

use engram_core::{
    AssemblyOmission, AssemblyOmissionReason, ContextAssembler, ContextAssemblyRequest,
    ContextAssemblyResult, ContextCandidate, ContextKind, ContextPriority, ContextProvenance,
    ContextQuota, ContextRef, ContextRefError, ContextRepresentations, Handoff, ObservationId,
    PageId,
};
use sha2::{Digest, Sha256};

use crate::error::{StoreError, StoreResult};
use crate::reader::ReaderPool;
use crate::scope::{ScopeName, lookup_existing_scopes};

/// An embedded query plus the exact triple its vectors were produced with.
///
/// Supplied only by callers that already hold an embedder. The hook path never
/// constructs one: SessionStart must not make a synchronous model call.
#[derive(Clone, Debug)]
pub struct QueryEmbedding {
    /// Query vector.
    pub vector: Vec<f32>,
    /// Embedding provider id, as stored on `page_embeddings`.
    pub provider: String,
    /// Embedding model id.
    pub model: String,
    /// Vector dimension.
    pub dim: u32,
}

/// Assembly options for one continuation package.
#[derive(Clone, Debug)]
pub struct HandoffContextRequest {
    /// Budget in selected-content UTF-8 bytes.
    pub budget: usize,
    /// Per-kind entry ceilings.
    pub quotas: Vec<ContextQuota>,
    /// Exact refs already present in the receiver's context.
    pub already_used: BTreeSet<ContextRef>,
    /// Optional embedded query enabling the hybrid retrieval leg.
    pub embedding: Option<QueryEmbedding>,
    /// Maximum retrieval candidates generated from the Handoff prose.
    pub retrieval_limit: usize,
}

/// Default retrieval-candidate ceiling for continuation assembly.
pub const DEFAULT_HANDOFF_RETRIEVAL_LIMIT: usize = 20;

/// Default per-kind entry ceilings for a continuation or query package.
///
/// Shared by `memory_query`, on-demand `memory_handoff_claim`, and automatic
/// SessionStart recovery. Passing an empty quota list instead would give every
/// kind an unlimited ceiling, so the SessionStart package would be assembled
/// under different rules than the identical on-demand claim — exactly the drift
/// this module exists to prevent.
pub const DEFAULT_WIKI_PAGE_QUOTA: usize = 6;
/// Default `session_page` ceiling. See [`DEFAULT_WIKI_PAGE_QUOTA`].
pub const DEFAULT_SESSION_PAGE_QUOTA: usize = 3;
/// Default `observation` ceiling. See [`DEFAULT_WIKI_PAGE_QUOTA`].
pub const DEFAULT_OBSERVATION_QUOTA: usize = 3;

/// Caller overrides for the per-kind ceilings. `None` keeps the default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ContextQuotaOverrides {
    /// Maximum `wiki_page` entries.
    pub wiki_page: Option<usize>,
    /// Maximum `session_page` entries.
    pub session_page: Option<usize>,
    /// Maximum `observation` entries.
    pub observation: Option<usize>,
}

/// Build the per-kind ceilings for one assembly.
#[must_use]
pub fn context_quotas(overrides: ContextQuotaOverrides) -> Vec<ContextQuota> {
    vec![
        ContextQuota {
            kind: ContextKind::WikiPage,
            maximum: overrides.wiki_page.unwrap_or(DEFAULT_WIKI_PAGE_QUOTA),
        },
        ContextQuota {
            kind: ContextKind::SessionPage,
            maximum: overrides.session_page.unwrap_or(DEFAULT_SESSION_PAGE_QUOTA),
        },
        ContextQuota {
            kind: ContextKind::Observation,
            maximum: overrides.observation.unwrap_or(DEFAULT_OBSERVATION_QUOTA),
        },
    ]
}

/// Assembled continuation package plus the reinforcement ids its caller owes
/// the retention model.
#[derive(Debug)]
pub struct HandoffContext {
    /// Package and content-free assembly trace.
    pub assembled: ContextAssemblyResult,
    /// Page revisions to bump exactly once each.
    pub access_bump_ids: Vec<PageId>,
}

/// Assemble the continuation package for one claimed Handoff.
///
/// Explicit publisher refs resolve first (each in its own encoded scope, with
/// no silent substitution), then retrieval candidates derived from the Handoff
/// prose join under the same quotas. Assembly never mutates the Handoff.
///
/// # Errors
/// Propagates reader and scope-resolution failures.
pub async fn assemble_handoff_context(
    reader: &ReaderPool,
    handoff: &Handoff,
    request: HandoffContextRequest,
) -> StoreResult<HandoffContext> {
    let mut omissions = Vec::new();
    let mut candidates = Vec::new();
    for resolved in resolve_context_refs(reader, &handoff.context_refs).await? {
        match resolved {
            ResolvedContextRef::Candidate(candidate) => candidates.push(candidate),
            ResolvedContextRef::Omission(omission) => omissions.push(omission),
        }
    }
    candidates.extend(
        handoff_retrieval_candidates(
            reader,
            handoff,
            request.embedding.as_ref(),
            request.retrieval_limit,
        )
        .await?,
    );
    let access_bump_ids = claim_access_bump_ids(&candidates);
    let mut assembled = ContextAssembler.assemble(
        candidates,
        ContextAssemblyRequest {
            budget: request.budget,
            quotas: request.quotas,
            already_used: request.already_used,
        },
    );
    assembled.trace.omissions.extend(omissions);
    Ok(HandoffContext {
        assembled,
        access_bump_ids,
    })
}

/// Retrieval leg of continuation assembly: the Handoff's own prose, reduced to
/// plain search terms, run against the project's compiled pages (with bounded
/// raw observations as the miss fallback).
async fn handoff_retrieval_candidates(
    reader: &ReaderPool,
    handoff: &Handoff,
    embedding: Option<&QueryEmbedding>,
    limit: usize,
) -> StoreResult<Vec<ContextCandidate>> {
    let query = handoff_retrieval_query(handoff);
    if query.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let page_hits = match embedding {
        Some(embedding) => {
            reader
                .hybrid_search(
                    handoff.workspace_id,
                    handoff.project_id,
                    query.clone(),
                    Some(embedding.vector.clone()),
                    embedding.provider.clone(),
                    embedding.model.clone(),
                    embedding.dim,
                    limit,
                )
                .await?
        }
        None => {
            reader
                .search_pages_for_project(
                    handoff.workspace_id,
                    handoff.project_id,
                    query.clone(),
                    limit,
                )
                .await?
        }
    };
    let observation_hits = if page_hits.is_empty() {
        reader
            .search_observations_for_project(handoff.workspace_id, handoff.project_id, query, limit)
            .await?
    } else {
        Vec::new()
    };
    let page_sources = reader
        .context_pages_by_ids(page_hits.iter().map(|hit| hit.id).collect())
        .await?;
    let observation_sources = reader
        .context_observations_by_ids(observation_hits.iter().map(|hit| hit.id).collect())
        .await?;
    let pages_by_id: HashMap<_, _> = page_sources
        .into_iter()
        .map(|source| (source.id, source))
        .collect();
    let observations_by_id: HashMap<_, _> = observation_sources
        .into_iter()
        .map(|source| (source.id, source))
        .collect();
    let mut candidates = Vec::new();
    for hit in page_hits {
        if let Some(source) = pages_by_id.get(&hit.id)
            && let Ok(candidate) = page_context_candidate(
                PageCandidateHit {
                    id: hit.id,
                    rank: hit.rank,
                    snippet: hit.snippet,
                    provenance: hit.provenance,
                },
                source,
            )
        {
            candidates.push(candidate);
        }
    }
    for hit in observation_hits {
        if let Some(source) = observations_by_id.get(&hit.id)
            && let Ok(candidate) = observation_context_candidate(hit, source)
        {
            candidates.push(candidate);
        }
    }
    Ok(candidates)
}

/// Resolve every publisher-selected reference against its own encoded scope,
/// batching the work into one lookup per distinct scope plus one read per
/// source table. Returns one outcome per input, in input order.
///
/// # Errors
/// Propagates reader and scope-resolution failures.
pub async fn resolve_context_refs(
    reader: &ReaderPool,
    references: &[ContextRef],
) -> StoreResult<Vec<ResolvedContextRef>> {
    if references.is_empty() {
        return Ok(Vec::new());
    }
    let scope_names = references.iter().map(scope_key).collect::<Vec<_>>();
    let scopes = lookup_existing_scopes(reader, &scope_names)
        .await
        .map_err(|error| StoreError::InvalidState(error.to_string()))?;
    let mut page_revisions = HashSet::new();
    let mut observation_ids = HashSet::new();
    for reference in references {
        match reference.kind() {
            ContextKind::WikiPage | ContextKind::SessionPage => {
                if let Some(revision) = reference.page_revision() {
                    page_revisions.insert(revision);
                }
            }
            ContextKind::Observation => {
                if let Some(identity) = reference.observation_id() {
                    observation_ids.insert(identity);
                }
            }
        }
    }
    let pages_by_id: HashMap<_, _> = reader
        .context_pages_by_ids(page_revisions.into_iter().collect())
        .await?
        .into_iter()
        .map(|source| (source.id, source))
        .collect();
    let observations_by_id: HashMap<_, _> = reader
        .context_observations_by_ids(observation_ids.into_iter().collect())
        .await?
        .into_iter()
        .map(|source| (source.id, source))
        .collect();
    Ok(references
        .iter()
        .map(|reference| {
            let scope = scopes.get(&scope_key(reference)).copied();
            match (scope, reference.kind()) {
                (None, _) => unresolved_ref(reference.clone()),
                (Some(scope), ContextKind::WikiPage | ContextKind::SessionPage) => {
                    resolve_page_ref(reference.clone(), scope, &pages_by_id)
                }
                (Some(scope), ContextKind::Observation) => {
                    resolve_observation_ref(reference.clone(), scope, &observations_by_id)
                }
            }
        })
        .collect())
}

/// One retrieval hit paired with the provenance that produced it.
#[derive(Debug)]
pub struct PageCandidateHit {
    /// Page version id the hit resolved to.
    pub id: PageId,
    /// Retrieval rank from the existing pipeline (lower is better for BM25).
    pub rank: f64,
    /// Snippet the search returned, still carrying `<mark>` wrappers.
    pub snippet: String,
    /// Which retrieval legs contributed this hit.
    pub provenance: Vec<String>,
}

fn clean_search_snippet(snippet: &str) -> String {
    snippet
        .replace("<mark>", "")
        .replace("</mark>", "")
        .trim()
        .to_string()
}

fn truncate_utf8_bytes(value: &str, maximum: usize) -> &str {
    let mut end = maximum.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn make_brief(title: &str, snippet: &str) -> String {
    if snippet.is_empty() {
        return title.to_string();
    }
    format!("{title} — {}", truncate_utf8_bytes(snippet, 160))
}

fn make_overview(title: &str, snippet: &str) -> String {
    if snippet.is_empty() {
        title.to_string()
    } else {
        format!("{title}\n\n{snippet}")
    }
}

/// Everything needed to materialize one [`ContextCandidate`].
pub struct ContextCandidateParts<'a> {
    /// Stable reference this candidate resolves to.
    pub context_ref: ContextRef,
    /// Source title.
    pub title: &'a str,
    /// Raw search snippet (may contain `<mark>` wrappers).
    pub snippet: &'a str,
    /// Selection priority compared before the retrieval score.
    pub priority: ContextPriority,
    /// Retrieval score.
    pub score: f64,
    /// Content-equality key used to collapse duplicates.
    pub deduplication_key: String,
    /// Retrieval legs that contributed this candidate.
    pub retrieval_sources: Vec<String>,
    /// Why this candidate was selected.
    pub selection_reason: &'a str,
    /// Full source body.
    pub body: &'a str,
}

/// Outcome of resolving one publisher-selected ContextRef.
pub enum ResolvedContextRef {
    /// The reference resolved exactly in its own encoded scope.
    Candidate(ContextCandidate),
    /// The reference could not be delivered; the reason is reported in the
    /// assembly trace without substituting another scope.
    Omission(AssemblyOmission),
}

/// Page revisions to reinforce for one claim, each at most once.
///
/// A publisher can repeat a ContextRef, and an explicitly referenced page can
/// also match the handoff prose, so the same revision arrives as several
/// candidates. The assembler collapses those into one package entry, and
/// `bump_access_for_pages` increments once per supplied id — without the
/// dedup a single claim could record one access per allowed reference plus
/// its retrieval hit, inflating the page's retention score.
#[must_use]
pub fn claim_access_bump_ids(candidates: &[ContextCandidate]) -> Vec<PageId> {
    let mut seen = HashSet::new();
    candidates
        .iter()
        .filter_map(|candidate| candidate.context_ref.page_revision())
        .filter(|revision| seen.insert(*revision))
        .collect()
}

fn scope_key(reference: &ContextRef) -> ScopeName {
    ScopeName::new(reference.workspace(), reference.project())
}

fn resolve_page_ref(
    reference: ContextRef,
    scope: crate::ResolvedScope,
    pages_by_id: &HashMap<PageId, crate::PageContextSource>,
) -> ResolvedContextRef {
    let Some(revision) = reference.page_revision() else {
        return unresolved_ref(reference);
    };
    let Some(path) = reference.page_path().cloned() else {
        return unresolved_ref(reference);
    };
    let Some(source) = pages_by_id.get(&revision) else {
        return unresolved_ref(reference);
    };
    if source.workspace_id != scope.workspace_id || source.project_id != scope.project_id {
        return unauthorized_ref(reference);
    }
    if source.path != path
        || source.workspace_name != reference.workspace()
        || source.project_name != reference.project()
    {
        return unauthorized_ref(reference);
    }
    let kind = if source.path.as_str().starts_with("sessions/") {
        ContextKind::SessionPage
    } else {
        ContextKind::WikiPage
    };
    if kind != reference.kind() {
        return unresolved_ref(reference);
    }
    match explicit_page_candidate(reference.clone(), source) {
        Ok(candidate) => ResolvedContextRef::Candidate(candidate),
        Err(_) => unresolved_ref(reference),
    }
}

fn resolve_observation_ref(
    reference: ContextRef,
    scope: crate::ResolvedScope,
    observations_by_id: &HashMap<ObservationId, crate::ObservationContextSource>,
) -> ResolvedContextRef {
    let Some(identity) = reference.observation_id() else {
        return unresolved_ref(reference);
    };
    let Some(source) = observations_by_id.get(&identity) else {
        return unresolved_ref(reference);
    };
    if source.workspace_id != scope.workspace_id || source.project_id != scope.project_id {
        return unauthorized_ref(reference);
    }
    if source.workspace_name != reference.workspace() || source.project_name != reference.project()
    {
        return unauthorized_ref(reference);
    }
    match explicit_observation_candidate(reference.clone(), source) {
        Ok(candidate) => ResolvedContextRef::Candidate(candidate),
        Err(_) => unresolved_ref(reference),
    }
}

fn unresolved_ref(context_ref: ContextRef) -> ResolvedContextRef {
    ResolvedContextRef::Omission(AssemblyOmission {
        context_ref,
        reason: AssemblyOmissionReason::UnresolvedContextRef,
    })
}

fn unauthorized_ref(context_ref: ContextRef) -> ResolvedContextRef {
    ResolvedContextRef::Omission(AssemblyOmission {
        context_ref,
        reason: AssemblyOmissionReason::UnauthorizedContextRef,
    })
}

/// Human-readable reason label for an omitted ContextRef.
#[must_use]
pub fn context_ref_omission_label(reason: AssemblyOmissionReason) -> &'static str {
    match reason {
        AssemblyOmissionReason::UnauthorizedContextRef => {
            "unauthorized in its encoded scope (no silent scope substitution)"
        }
        AssemblyOmissionReason::UnresolvedContextRef => {
            "missing, deleted, or stale in its encoded scope"
        }
        _ => "unresolved",
    }
}

/// Plain search terms derived from a Handoff's prose for the retrieval leg.
#[must_use]
pub fn handoff_retrieval_query(handoff: &Handoff) -> String {
    let questions = handoff.open_questions.join(" ");
    let steps = handoff.next_steps.join(" ");
    let raw = [
        handoff.brief.as_str(),
        handoff.summary.as_str(),
        &questions,
        &steps,
    ]
    .into_iter()
    .filter(|part| !part.trim().is_empty())
    .collect::<Vec<_>>()
    .join(" ");
    // The store owns what counts as FTS5 metasyntax; handoff text is prose
    // and must never be read as a query expression. Routing happens once, in
    // the store's routed search.
    crate::natural_language_terms(&raw)
}

fn explicit_page_candidate(
    context_ref: ContextRef,
    source: &crate::PageContextSource,
) -> Result<ContextCandidate, ContextRefError> {
    Ok(context_candidate(ContextCandidateParts {
        context_ref,
        title: &source.title,
        snippet: "",
        priority: ContextPriority::Explicit,
        score: 0.0,
        deduplication_key: format!("sha256:{}", source.body_sha256),
        retrieval_sources: vec!["handoff_explicit_ref".into()],
        selection_reason: "handoff_explicit_ref",
        body: &source.body,
    }))
}

fn explicit_observation_candidate(
    context_ref: ContextRef,
    source: &crate::ObservationContextSource,
) -> Result<ContextCandidate, ContextRefError> {
    Ok(context_candidate(ContextCandidateParts {
        context_ref,
        title: &source.title,
        snippet: "",
        priority: ContextPriority::Explicit,
        score: 0.0,
        deduplication_key: format!("sha256:{:x}", Sha256::digest(source.body.as_bytes())),
        retrieval_sources: vec!["handoff_explicit_ref".into()],
        selection_reason: "handoff_explicit_ref",
        body: &source.body,
    }))
}

/// Build one [`ContextCandidate`] from already-resolved source parts.
#[must_use]
pub fn context_candidate(parts: ContextCandidateParts<'_>) -> ContextCandidate {
    let clean_snippet = clean_search_snippet(parts.snippet);
    ContextCandidate {
        provenance: parts
            .retrieval_sources
            .into_iter()
            .map(|source| ContextProvenance {
                source,
                context_ref: parts.context_ref.clone(),
            })
            .collect(),
        context_ref: parts.context_ref,
        title: parts.title.to_string(),
        priority: parts.priority,
        score: parts.score,
        deduplication_key: parts.deduplication_key,
        selection_reason: parts.selection_reason.to_string(),
        representations: ContextRepresentations {
            brief: make_brief(parts.title, &clean_snippet),
            overview: make_overview(parts.title, &clean_snippet),
            full_evidence: format!("# {}\n\n{}", parts.title, parts.body),
        },
    }
}

/// Build a retrieval candidate for one page hit.
///
/// # Errors
/// Returns [`ContextRefError`] when the source cannot form a stable ref.
pub fn page_context_candidate(
    hit: PageCandidateHit,
    source: &crate::PageContextSource,
) -> Result<ContextCandidate, ContextRefError> {
    let context_ref = ContextRef::page(
        source.workspace_name.clone(),
        source.project_name.clone(),
        source.path.clone(),
        source.id,
    )?;
    Ok(context_candidate(ContextCandidateParts {
        context_ref,
        title: &source.title,
        snippet: &hit.snippet,
        priority: ContextPriority::Retrieved,
        score: hit.rank,
        deduplication_key: format!("sha256:{}", source.body_sha256),
        retrieval_sources: hit.provenance,
        selection_reason: "existing_hybrid_retrieval_rank",
        body: &source.body,
    }))
}

/// Build a retrieval candidate for one observation hit.
///
/// # Errors
/// Returns [`ContextRefError`] when the source cannot form a stable ref.
pub fn observation_context_candidate(
    hit: crate::ObservationHit,
    source: &crate::ObservationContextSource,
) -> Result<ContextCandidate, ContextRefError> {
    let context_ref = ContextRef::observation(
        source.workspace_name.clone(),
        source.project_name.clone(),
        source.id,
    )?;
    Ok(context_candidate(ContextCandidateParts {
        context_ref,
        title: &source.title,
        snippet: &hit.snippet,
        priority: ContextPriority::Retrieved,
        score: hit.rank,
        deduplication_key: format!("sha256:{:x}", Sha256::digest(source.body.as_bytes())),
        retrieval_sources: hit.provenance,
        selection_reason: "raw_observation_fallback_rank",
        body: &source.body,
    }))
}

/// Join retrieval hits with their loaded sources into ordered candidates.
///
/// # Errors
/// Returns [`StoreError::MalformedRecord`] when a hit has no loaded source.
pub fn build_context_candidates(
    page_hits: Vec<PageCandidateHit>,
    observation_hits: Vec<crate::ObservationHit>,
    pages_by_id: &HashMap<PageId, crate::PageContextSource>,
    observations_by_id: &HashMap<ObservationId, crate::ObservationContextSource>,
) -> StoreResult<Vec<ContextCandidate>> {
    let mut candidates = Vec::with_capacity(page_hits.len() + observation_hits.len());
    for hit in page_hits {
        let source = pages_by_id.get(&hit.id).ok_or_else(|| {
            StoreError::MalformedRecord(format!(
                "retrieval candidate page revision {} is missing",
                hit.id
            ))
        })?;
        candidates.push(
            page_context_candidate(hit, source)
                .map_err(|error| StoreError::MalformedRecord(error.to_string()))?,
        );
    }
    for hit in observation_hits {
        let source = observations_by_id.get(&hit.id).ok_or_else(|| {
            StoreError::MalformedRecord(format!(
                "retrieval candidate observation revision {} is missing",
                hit.id
            ))
        })?;
        candidates.push(
            observation_context_candidate(hit, source)
                .map_err(|error| StoreError::MalformedRecord(error.to_string()))?,
        );
    }
    Ok(candidates)
}

#[cfg(test)]
mod tests {
    use engram_core::{ObservationId, PagePath};

    use super::*;

    /// One claim must reinforce each consumed page revision exactly once.
    /// A publisher may repeat a reference, and an explicitly referenced page
    /// can also match the handoff prose; `bump_access_for_pages` increments
    /// per supplied id, so without the dedup one claim inflates a page's
    /// retention score by up to the reference limit plus its retrieval hit.
    #[test]
    fn claim_access_bump_records_each_page_revision_once() {
        fn page_candidate(
            path: &str,
            revision: PageId,
            priority: ContextPriority,
        ) -> ContextCandidate {
            context_candidate(ContextCandidateParts {
                context_ref: ContextRef::page(
                    "default",
                    "scratch",
                    PagePath::new(path).unwrap(),
                    revision,
                )
                .unwrap(),
                title: "t",
                snippet: "",
                priority,
                score: 0.0,
                deduplication_key: format!("sha256:{path}"),
                retrieval_sources: vec!["fts".into()],
                selection_reason: "test",
                body: "b",
            })
        }

        let repeated = PageId::new();
        let other = PageId::new();
        let observation = context_candidate(ContextCandidateParts {
            context_ref: ContextRef::observation("default", "scratch", ObservationId::new())
                .unwrap(),
            title: "o",
            snippet: "",
            priority: ContextPriority::Retrieved,
            score: -3.0,
            deduplication_key: "sha256:o".into(),
            retrieval_sources: vec!["fts".into()],
            selection_reason: "test",
            body: "b",
        });
        let candidates = vec![
            // The publisher named the same revision twice ...
            page_candidate("notes/a.md", repeated, ContextPriority::Explicit),
            page_candidate("notes/a.md", repeated, ContextPriority::Explicit),
            page_candidate("notes/b.md", other, ContextPriority::Explicit),
            // ... and retrieval surfaced one of them again.
            page_candidate("notes/a.md", repeated, ContextPriority::Retrieved),
            observation,
        ];

        assert_eq!(
            claim_access_bump_ids(&candidates),
            vec![repeated, other],
            "one bump per revision, in first-seen order, observations excluded"
        );
        assert!(claim_access_bump_ids(&[]).is_empty());
    }
}
