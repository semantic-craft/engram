use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};

use super::{ContextKind, ContextRef};
use crate::PagePath;

/// Public accounting unit: one selected-content UTF-8 byte.
pub const CONTEXT_BUDGET_UNIT: &str = "utf8_bytes";

/// Engram-owned representation tiers.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ContextDetailTier {
    /// Cheap identifying coverage.
    Brief,
    /// Search-focused synopsis.
    Overview,
    /// Complete evidence body.
    FullEvidence,
}

/// One retrieval source that contributed to a candidate.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ContextProvenance {
    /// Retrieval leg or union source.
    pub source: String,
    /// Exact source reference contributed by that leg.
    pub context_ref: ContextRef,
}

/// Tiered representations of one source revision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextRepresentations {
    /// Cheap identifying coverage.
    pub brief: String,
    /// Search-focused synopsis.
    pub overview: String,
    /// Complete evidence.
    pub full_evidence: String,
}

/// Typed input from the existing retrieval pipeline.
#[derive(Clone, Debug, PartialEq)]
pub struct ContextCandidate {
    /// Preferred canonical source.
    pub context_ref: ContextRef,
    /// Human-readable source title.
    pub title: String,
    /// Lower is better, matching current search ranks.
    pub score: f64,
    /// Content-equivalence key.
    pub deduplication_key: String,
    /// Contributing retrieval sources.
    pub provenance: Vec<ContextProvenance>,
    /// Content-free selection rationale.
    pub selection_reason: String,
    /// Tiered source representations.
    pub representations: ContextRepresentations,
}

/// Per-kind selected-entry ceiling.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextQuota {
    /// Context kind.
    pub kind: ContextKind,
    /// Maximum selected entries.
    pub maximum: usize,
}

/// Controls for one assembly pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextAssemblyRequest {
    /// Maximum selected-content UTF-8 bytes.
    pub budget: usize,
    /// Per-kind ceilings.
    pub quotas: Vec<ContextQuota>,
    /// Exact revisions already in caller context.
    pub already_used: BTreeSet<ContextRef>,
}

/// Selected-content truncation state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextTruncation {
    /// Tier included in full.
    None,
    /// Tier shortened to available budget.
    Truncated,
}

/// One ordered package entry.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ContextEntry {
    /// Stable source reference.
    pub context_ref: ContextRef,
    /// Owning workspace name.
    pub workspace: String,
    /// Owning project name.
    pub project: String,
    /// Relative page path for navigable page entries.
    pub page_path: Option<PagePath>,
    /// Human-readable source title.
    pub title: String,
    /// Source kind.
    pub kind: ContextKind,
    /// Exact source revision.
    pub source_revision: String,
    /// Deepest included tier.
    pub detail_tier: ContextDetailTier,
    /// Selected representation.
    pub content: String,
    /// Retained retrieval provenance.
    pub provenance: Vec<ContextProvenance>,
    /// Original retrieval score.
    pub score: f64,
    /// Content-free rationale.
    pub selection_reason: String,
    /// Explicit truncation marker.
    pub truncation: ContextTruncation,
    /// Selected content bytes.
    pub estimated_consumption: usize,
}

/// Ordered selected context and accounting.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ContextPackage {
    /// Requested maximum.
    pub budget: usize,
    /// Accounting unit.
    pub budget_unit: &'static str,
    /// Selected bytes.
    pub estimated_consumption: usize,
    /// Ordered entries.
    pub entries: Vec<ContextEntry>,
}

/// Why a candidate was omitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AssemblyOmissionReason {
    /// Candidate has no content that can be selected.
    EmptyRepresentation,
    /// Equivalent content already represented.
    Deduplicated,
    /// Exact revision already in caller context.
    AlreadyUsed,
    /// Kind ceiling reached.
    QuotaExceeded,
    /// Remaining budget exhausted.
    BudgetExhausted,
    /// Candidate larger than total budget.
    Oversized,
    /// Explicit ContextRef is missing, deleted, or revision-stale in its
    /// encoded existing scope.
    UnresolvedContextRef,
    /// Explicit ContextRef's encoded scope does not own the source. Another
    /// scope was not substituted.
    UnauthorizedContextRef,
}

/// Content-free omission marker.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AssemblyOmission {
    /// Omitted source.
    pub context_ref: ContextRef,
    /// Deterministic reason.
    pub reason: AssemblyOmissionReason,
}

/// Content-free assembly diagnostics.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AssemblyTrace {
    /// Candidates received.
    pub candidate_count: usize,
    /// Equivalent candidates collapsed.
    pub deduplicated_count: usize,
    /// Exact revisions already used.
    pub already_used_count: usize,
    /// Candidates rejected by quotas.
    pub quota_omitted_count: usize,
    /// Final entry counts by tier.
    pub selected_tiers: BTreeMap<ContextDetailTier, usize>,
    /// Explicit omissions.
    pub omissions: Vec<AssemblyOmission>,
    /// Requested maximum.
    pub requested_budget: usize,
    /// Selected bytes.
    pub estimated_consumption: usize,
    /// Accounting unit.
    pub budget_unit: &'static str,
}

/// Public assembler output.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ContextAssemblyResult {
    /// Ordered budgeted package.
    pub package: ContextPackage,
    /// Content-free trace.
    pub trace: AssemblyTrace,
}

/// Shared deterministic context assembler.
#[derive(Clone, Copy, Debug, Default)]
pub struct ContextAssembler;

impl ContextAssembler {
    /// Assemble breadth first, then deepen in stable rank order.
    #[must_use]
    pub fn assemble(
        &self,
        candidates: Vec<ContextCandidate>,
        request: ContextAssemblyRequest,
    ) -> ContextAssemblyResult {
        let candidate_count = candidates.len();
        let prepared = deduplicate(candidates);
        let mut state = AssemblyState::new(request, prepared.omissions);
        state.select_breadth(prepared.candidates);
        state.deepen_if_complete();
        state.finish(candidate_count, prepared.deduplicated_count)
    }
}

struct PreparedCandidates {
    candidates: Vec<ContextCandidate>,
    omissions: Vec<AssemblyOmission>,
    deduplicated_count: usize,
}

fn deduplicate(mut candidates: Vec<ContextCandidate>) -> PreparedCandidates {
    candidates.sort_by(candidate_order);
    let mut prepared = PreparedCandidates {
        candidates: Vec::new(),
        omissions: Vec::new(),
        deduplicated_count: 0,
    };
    let mut index = HashMap::<String, usize>::new();
    for mut candidate in candidates {
        canonicalize_provenance(&mut candidate.provenance);
        let key = candidate_deduplication_key(&candidate);
        if let Some(&existing) = index.get(&key) {
            prepared.deduplicated_count += 1;
            prepared.candidates[existing]
                .provenance
                .extend(candidate.provenance);
            canonicalize_provenance(&mut prepared.candidates[existing].provenance);
            prepared.omissions.push(AssemblyOmission {
                context_ref: candidate.context_ref,
                reason: AssemblyOmissionReason::Deduplicated,
            });
        } else {
            index.insert(key, prepared.candidates.len());
            prepared.candidates.push(candidate);
        }
    }
    prepared
}

fn candidate_deduplication_key(candidate: &ContextCandidate) -> String {
    if candidate.deduplication_key.is_empty() {
        candidate.context_ref.to_string()
    } else {
        candidate.deduplication_key.clone()
    }
}

struct AssemblyState {
    budget: usize,
    quotas: BTreeMap<ContextKind, usize>,
    already_used: BTreeSet<ContextRef>,
    counts: BTreeMap<ContextKind, usize>,
    entries: Vec<ContextEntry>,
    selected: Vec<ContextCandidate>,
    oversized: Vec<ContextCandidate>,
    omissions: Vec<AssemblyOmission>,
    consumed: usize,
    already_used_count: usize,
    quota_omitted_count: usize,
}

impl AssemblyState {
    fn new(request: ContextAssemblyRequest, omissions: Vec<AssemblyOmission>) -> Self {
        Self {
            budget: request.budget,
            quotas: request
                .quotas
                .into_iter()
                .map(|quota| (quota.kind, quota.maximum))
                .collect(),
            already_used: request.already_used,
            counts: BTreeMap::new(),
            entries: Vec::new(),
            selected: Vec::new(),
            oversized: Vec::new(),
            omissions,
            consumed: 0,
            already_used_count: 0,
            quota_omitted_count: 0,
        }
    }

    fn select_breadth(&mut self, candidates: Vec<ContextCandidate>) {
        for candidate in candidates {
            self.select_candidate(candidate);
        }
        self.include_oversized_fallback();
        self.omissions
            .extend(self.oversized.drain(..).map(|candidate| AssemblyOmission {
                context_ref: candidate.context_ref,
                reason: AssemblyOmissionReason::Oversized,
            }));
    }

    fn select_candidate(&mut self, candidate: ContextCandidate) {
        if self.already_used.contains(&candidate.context_ref) {
            self.already_used_count += 1;
            self.omit(candidate.context_ref, AssemblyOmissionReason::AlreadyUsed);
            return;
        }
        let kind = candidate.context_ref.kind();
        let maximum = self.quotas.get(&kind).copied().unwrap_or(usize::MAX);
        if self.counts.get(&kind).copied().unwrap_or(0) >= maximum {
            self.quota_omitted_count += 1;
            self.omit(candidate.context_ref, AssemblyOmissionReason::QuotaExceeded);
            return;
        }
        let brief_cost = estimate(&candidate.representations.brief);
        if brief_cost <= self.budget.saturating_sub(self.consumed) {
            let context_ref = candidate.context_ref.clone();
            if !self.include_brief(candidate, ContextTruncation::None) {
                self.omit(context_ref, AssemblyOmissionReason::EmptyRepresentation);
            }
        } else if brief_cost > self.budget {
            self.oversized.push(candidate);
        } else {
            self.omit(
                candidate.context_ref,
                AssemblyOmissionReason::BudgetExhausted,
            );
        }
    }

    fn include_brief(
        &mut self,
        candidate: ContextCandidate,
        truncation: ContextTruncation,
    ) -> bool {
        let content = if truncation == ContextTruncation::Truncated {
            truncate_utf8(&candidate.representations.brief, self.budget)
        } else {
            candidate.representations.brief.clone()
        };
        if content.is_empty() {
            return false;
        }
        self.consumed += estimate(&content);
        *self.counts.entry(candidate.context_ref.kind()).or_default() += 1;
        self.entries.push(entry_for(
            &candidate,
            ContextDetailTier::Brief,
            content,
            truncation,
        ));
        self.selected.push(candidate);
        true
    }

    fn include_oversized_fallback(&mut self) {
        if self.entries.is_empty() && self.budget > 0 && !self.oversized.is_empty() {
            let candidate = self.oversized.remove(0);
            let context_ref = candidate.context_ref.clone();
            if !self.include_brief(candidate, ContextTruncation::Truncated) {
                self.omit(context_ref, AssemblyOmissionReason::Oversized);
            }
        }
    }

    fn omit(&mut self, context_ref: ContextRef, reason: AssemblyOmissionReason) {
        self.omissions.push(AssemblyOmission {
            context_ref,
            reason,
        });
    }

    fn deepen_if_complete(&mut self) {
        if self
            .entries
            .iter()
            .any(|entry| entry.truncation == ContextTruncation::Truncated)
        {
            return;
        }
        deepen(
            &self.selected,
            &mut self.entries,
            &mut self.consumed,
            self.budget,
            ContextDetailTier::Overview,
        );
        deepen(
            &self.selected,
            &mut self.entries,
            &mut self.consumed,
            self.budget,
            ContextDetailTier::FullEvidence,
        );
    }

    fn finish(self, candidate_count: usize, deduplicated_count: usize) -> ContextAssemblyResult {
        debug_assert_eq!(
            self.consumed,
            self.entries
                .iter()
                .map(|entry| entry.estimated_consumption)
                .sum::<usize>()
        );
        debug_assert!(self.consumed <= self.budget);
        let selected_tiers = selected_tier_counts(&self.entries);
        ContextAssemblyResult {
            package: ContextPackage {
                budget: self.budget,
                budget_unit: CONTEXT_BUDGET_UNIT,
                estimated_consumption: self.consumed,
                entries: self.entries,
            },
            trace: AssemblyTrace {
                candidate_count,
                deduplicated_count,
                already_used_count: self.already_used_count,
                quota_omitted_count: self.quota_omitted_count,
                selected_tiers,
                omissions: self.omissions,
                requested_budget: self.budget,
                estimated_consumption: self.consumed,
                budget_unit: CONTEXT_BUDGET_UNIT,
            },
        }
    }
}

fn selected_tier_counts(entries: &[ContextEntry]) -> BTreeMap<ContextDetailTier, usize> {
    let mut selected_tiers = BTreeMap::new();
    for entry in entries {
        *selected_tiers.entry(entry.detail_tier).or_default() += 1;
    }
    selected_tiers
}

fn candidate_order(a: &ContextCandidate, b: &ContextCandidate) -> std::cmp::Ordering {
    a.score
        .total_cmp(&b.score)
        .then_with(|| a.context_ref.kind().cmp(&b.context_ref.kind()))
        .then_with(|| a.context_ref.cmp(&b.context_ref))
}

fn canonicalize_provenance(value: &mut Vec<ContextProvenance>) {
    value.sort();
    value.dedup();
}

fn estimate(value: &str) -> usize {
    value.len()
}

fn truncate_utf8(value: &str, maximum: usize) -> String {
    let mut end = maximum.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn entry_for(
    candidate: &ContextCandidate,
    tier: ContextDetailTier,
    content: String,
    truncation: ContextTruncation,
) -> ContextEntry {
    ContextEntry {
        context_ref: candidate.context_ref.clone(),
        workspace: candidate.context_ref.workspace().to_string(),
        project: candidate.context_ref.project().to_string(),
        page_path: candidate.context_ref.page_path().cloned(),
        title: candidate.title.clone(),
        kind: candidate.context_ref.kind(),
        source_revision: candidate.context_ref.source_revision(),
        detail_tier: tier,
        estimated_consumption: estimate(&content),
        content,
        provenance: candidate.provenance.clone(),
        score: candidate.score,
        selection_reason: candidate.selection_reason.clone(),
        truncation,
    }
}

fn deepen(
    candidates: &[ContextCandidate],
    entries: &mut [ContextEntry],
    consumed: &mut usize,
    budget: usize,
    target: ContextDetailTier,
) {
    for (candidate, entry) in candidates.iter().zip(entries.iter_mut()) {
        let content = match target {
            ContextDetailTier::Brief => &candidate.representations.brief,
            ContextDetailTier::Overview => &candidate.representations.overview,
            ContextDetailTier::FullEvidence => &candidate.representations.full_evidence,
        };
        let next = estimate(content);
        let base = consumed.saturating_sub(entry.estimated_consumption);
        if base.saturating_add(next) <= budget {
            *consumed = base + next;
            entry.content.clone_from(content);
            entry.estimated_consumption = next;
            entry.detail_tier = target;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ObservationId, PageId};

    fn reference(kind: ContextKind, identity: &str) -> ContextRef {
        match kind {
            ContextKind::WikiPage | ContextKind::SessionPage => {
                let path = if kind == ContextKind::SessionPage {
                    PagePath::new(format!("sessions/{identity}.md")).unwrap()
                } else {
                    PagePath::new(format!("notes/{identity}.md")).unwrap()
                };
                ContextRef::page("default", "engram", path, PageId::new()).unwrap()
            }
            ContextKind::Observation => {
                ContextRef::observation("default", "engram", ObservationId::new()).unwrap()
            }
        }
    }

    fn candidate(
        kind: ContextKind,
        identity: &str,
        score: f64,
        brief: &str,
        overview: &str,
        full: &str,
    ) -> ContextCandidate {
        let context_ref = reference(kind, identity);
        ContextCandidate {
            context_ref: context_ref.clone(),
            title: identity.into(),
            score,
            deduplication_key: format!("body:{identity}"),
            provenance: vec![ContextProvenance {
                source: "fts".into(),
                context_ref,
            }],
            selection_reason: "retrieval_rank".into(),
            representations: ContextRepresentations {
                brief: brief.into(),
                overview: overview.into(),
                full_evidence: full.into(),
            },
        }
    }

    fn request(budget: usize) -> ContextAssemblyRequest {
        ContextAssemblyRequest {
            budget,
            quotas: [
                ContextKind::WikiPage,
                ContextKind::SessionPage,
                ContextKind::Observation,
            ]
            .into_iter()
            .map(|kind| ContextQuota { kind, maximum: 10 })
            .collect(),
            already_used: BTreeSet::new(),
        }
    }

    #[test]
    fn assembler_contract_is_budgeted_breadth_first_deduplicated_and_deterministic() {
        let first = candidate(ContextKind::WikiPage, "a", 0.0, "a", "AAAA", "AAAAAAAA");
        let quota_peer = candidate(ContextKind::WikiPage, "b", 1.0, "b", "BBBB", "BBBBBBBB");
        let observation = candidate(ContextKind::Observation, "c", 2.0, "c", "CCCC", "CCCCCCCC");
        let used = candidate(ContextKind::SessionPage, "d", 3.0, "d", "DDDD", "DDDDDDDD");
        let mut duplicate = candidate(ContextKind::WikiPage, "copy", 4.0, "a", "AAAA", "AAAAAAAA");
        duplicate.deduplication_key = first.deduplication_key.clone();
        duplicate.provenance[0].source = "vector".into();
        let candidates = vec![duplicate, used.clone(), observation, quota_peer, first];
        let mut req = request(5);
        req.quotas = vec![
            ContextQuota {
                kind: ContextKind::WikiPage,
                maximum: 1,
            },
            ContextQuota {
                kind: ContextKind::Observation,
                maximum: 1,
            },
            ContextQuota {
                kind: ContextKind::SessionPage,
                maximum: 1,
            },
        ];
        req.already_used.insert(used.context_ref);

        let left = ContextAssembler.assemble(candidates.clone(), req.clone());
        let right = ContextAssembler.assemble(candidates.into_iter().rev().collect(), req);
        assert_eq!(left, right, "input order must not affect the package");
        assert_eq!(left.package.estimated_consumption, 5);
        assert_eq!(
            left.package.entries.len(),
            2,
            "brief coverage precedes depth"
        );
        assert_eq!(
            left.package.entries[0].detail_tier,
            ContextDetailTier::Overview
        );
        assert_eq!(
            left.package.entries[1].detail_tier,
            ContextDetailTier::Brief
        );
        assert_eq!(left.package.entries[0].provenance.len(), 2);
        assert_eq!(left.trace.deduplicated_count, 1);
        assert_eq!(left.trace.quota_omitted_count, 1);
        assert_eq!(left.trace.already_used_count, 1);

        let fully_deepened = ContextAssembler.assemble(
            vec![candidate(
                ContextKind::WikiPage,
                "deep",
                0.0,
                "D",
                "DD",
                "DDD",
            )],
            request(3),
        );
        assert_eq!(
            fully_deepened.package.entries[0].detail_tier,
            ContextDetailTier::FullEvidence
        );

        let oversized = ContextAssembler.assemble(
            vec![
                candidate(
                    ContextKind::WikiPage,
                    "large",
                    0.0,
                    "ééé",
                    "overview",
                    "full",
                ),
                candidate(ContextKind::Observation, "small", 1.0, "ok", "ok", "ok"),
            ],
            request(5),
        );
        assert_eq!(oversized.package.entries[0].content, "ok");
        assert_eq!(
            oversized.package.entries[0].truncation,
            ContextTruncation::None
        );
        assert!(oversized.trace.omissions.iter().any(|omission| {
            omission.reason == AssemblyOmissionReason::Oversized
                && omission.context_ref.kind() == ContextKind::WikiPage
        }));
        assert!(oversized.package.estimated_consumption <= oversized.package.budget);

        let only_oversized = ContextAssembler.assemble(
            vec![candidate(
                ContextKind::WikiPage,
                "only-large",
                0.0,
                "ééé",
                "overview",
                "full",
            )],
            request(5),
        );
        assert_eq!(only_oversized.package.entries[0].content, "éé");
        assert_eq!(
            only_oversized.package.entries[0].truncation,
            ContextTruncation::Truncated
        );

        let one_byte = ContextAssembler.assemble(
            vec![candidate(
                ContextKind::WikiPage,
                "one-byte",
                0.0,
                "éé",
                "overview",
                "full",
            )],
            request(1),
        );
        assert!(one_byte.package.entries.is_empty());
        assert_eq!(one_byte.package.estimated_consumption, 0);
        assert_eq!(one_byte.trace.omissions.len(), 1);
        assert_eq!(
            one_byte.trace.omissions[0].reason,
            AssemblyOmissionReason::Oversized
        );

        let two_bytes = ContextAssembler.assemble(
            vec![candidate(
                ContextKind::WikiPage,
                "two-bytes",
                0.0,
                "éé",
                "overview",
                "full",
            )],
            request(2),
        );
        assert_eq!(two_bytes.package.entries[0].content, "é");
        assert_eq!(two_bytes.package.estimated_consumption, 2);
        assert_eq!(
            two_bytes.package.entries[0].truncation,
            ContextTruncation::Truncated
        );

        let empty = ContextAssembler.assemble(
            vec![candidate(
                ContextKind::Observation,
                "empty",
                0.0,
                "",
                "",
                "",
            )],
            request(10),
        );
        assert_eq!(empty.trace.candidate_count, 1);
        assert!(empty.package.entries.is_empty());
        assert_eq!(empty.trace.omissions.len(), 1);
        assert_eq!(
            empty.trace.omissions[0].reason,
            AssemblyOmissionReason::EmptyRepresentation
        );
    }
}
