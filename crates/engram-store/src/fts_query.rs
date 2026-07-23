//! FTS5 `MATCH` query preparation for user/agent-supplied search text.
//!
//! FTS5 treats `column:term` as a column-qualified search. Natural-language
//! queries that contain bare colons (`pick: handoff`, `memory: bootstrap`) make
//! SQLite error with `no such column: pick` because only `title` and `body`
//! exist on the FTS tables. Unknown bare column syntax is neutralised without
//! discarding deliberate FTS operators such as `OR`.

/// Sanitize free-text for use in `WHERE pages_fts MATCH ?`.
///
/// Returns an empty string when `raw` is empty/whitespace-only; callers
/// should skip the SQL query in that case.
///
/// Bare multi-word queries are joined with **`OR`**, not the FTS5 default
/// (`AND`). A natural-language query like "cross project search strategy"
/// otherwise requires every word to co-occur in one page — near-zero recall
/// for anything but single keywords. With `OR` + bm25 ranking (callers
/// `ORDER BY rank`), the best-matching pages still surface first. When the
/// caller supplies explicit FTS5 syntax (`OR` / `AND` / `NOT` / `NEAR` /
/// quoted phrases / parens) we preserve it verbatim instead.
#[must_use]
pub fn prepare_fts5_query(raw: &str) -> String {
    let explicit_syntax = raw.contains('"')
        || raw.contains('(')
        || raw.contains(')')
        || raw
            .split_whitespace()
            .any(|t| matches!(t, "OR" | "AND" | "NOT" | "NEAR"));
    let tokens: Vec<String> = raw
        .split_whitespace()
        .flat_map(prepare_fts5_token)
        .collect();
    if tokens.is_empty() {
        return String::new();
    }
    let separator = if explicit_syntax { " " } else { " OR " };
    tokens.join(separator)
}

fn prepare_fts5_token(token: &str) -> Vec<String> {
    if has_unknown_bare_column(token) {
        return token
            .replace(':', " ")
            .split_whitespace()
            .map(quote_fts5_token)
            .collect();
    }

    if should_quote_fts5_token(token) {
        vec![quote_fts5_token(token)]
    } else {
        vec![token.to_string()]
    }
}

fn has_unknown_bare_column(token: &str) -> bool {
    token.contains(':')
        && !token.contains('"')
        && !token.starts_with("title:")
        && !token.starts_with("body:")
}

fn should_quote_fts5_token(token: &str) -> bool {
    if token.starts_with('"') && token.ends_with('"') {
        return false;
    }
    // Quote any token carrying ASCII punctuation so FTS5 treats it as a literal
    // phrase instead of erroring on its query grammar — e.g. a filename like
    // `current.md` otherwise yields `fts5: syntax error near "."`. A trailing
    // `*` (the FTS5 prefix operator) is allowed through bare; accented letters
    // and digits are unicode (not ASCII punctuation) so recall keeps accents.
    let core = token.strip_suffix('*').unwrap_or(token);
    // `:` is column syntax (handled by `has_unknown_bare_column`, or preserved
    // for known `title:`/`body:` columns) — it must not trigger quoting here.
    core.chars().any(|c| c.is_ascii_punctuation() && c != ':')
}

fn quote_fts5_token(token: &str) -> String {
    // FTS5 escapes `"` by doubling it. A token carrying a literal quote is an
    // explicit-phrase fragment — keep the simple escaped form (don't expand it).
    if token.contains('"') {
        return format!("\"{}\"", token.replace('"', "\"\""));
    }
    // Otherwise emit BOTH the whole token and a punctuation-stripped sub-token
    // phrase, OR'd, because the content tokenizer and the path index disagree
    // on punctuation:
    //   tokenize = "unicode61 remove_diacritics 2 tokenchars '/_-'"
    // keeps `/ _ -` INSIDE tokens (so a body mention of `engram` indexes as
    // the single token `engram`), while `ops::path_search_text` pre-expands
    // `/ . - _` to spaces in the path index (so a path `ui-refresh-…` indexes
    // the sub-tokens `ui`, `refresh`, …). `.` is a separator either way.
    // Neither form alone matches both: `"engram"` matches the content token
    // but not the split path index; `"ai memory"` matches the path but not the
    // content token. OR-ing the two makes a search for `engram` / `ui-refresh`
    // hit whichever surface indexed it. (Quoting both also neutralises the
    // punctuation that would otherwise be FTS5 query grammar — the original
    // `current.md` → `syntax error` bug.) With no punctuation the two coincide
    // and we emit a single phrase.
    let split = token
        .chars()
        .map(|c| if c.is_ascii_punctuation() { ' ' } else { c })
        .collect::<String>();
    let split = split.split_whitespace().collect::<Vec<_>>().join(" ");
    if split.is_empty() || split == token {
        format!("\"{token}\"")
    } else {
        format!("(\"{token}\" OR \"{split}\")")
    }
}

/// Per-leg MATCH/LIKE inputs for CJK-aware routed search (#14).
///
/// unicode61 keeps word semantics for Latin text but tokenizes a CJK run as
/// one long token (zero recall for Chinese queries); the trigram shadow
/// (`pages_fts_cjk`) gives ≥3-char CJK terms substring MATCH + bm25 +
/// snippet, but by design cannot match anything shorter than 3 chars — and
/// two-character words are the most common shape of a Chinese query. The
/// router therefore splits one user query into up to three legs.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RoutedFtsQuery {
    /// `MATCH` string for the unicode61 `pages_fts` leg (non-CJK terms, or
    /// the whole query when the caller wrote explicit FTS5 syntax). Empty →
    /// skip the leg.
    pub unicode: String,
    /// `MATCH` string for the trigram `pages_fts_cjk` leg (phrase-quoted
    /// CJK-bearing terms of ≥3 chars). Empty → skip the leg.
    pub trigram: String,
    /// Raw 1–2 char CJK-bearing terms for the `LIKE '%term%'` fallback leg
    /// over `pages` directly (trigram cannot match them; a linear scan is
    /// sub-10ms at personal-wiki scale). Callers must LIKE-escape.
    pub like_terms: Vec<String>,
}

impl RoutedFtsQuery {
    /// True when no leg has anything to run (empty/whitespace query).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.unicode.is_empty() && self.trigram.is_empty() && self.like_terms.is_empty()
    }
}

fn has_cjk(s: &str) -> bool {
    s.chars().any(is_cjk)
}

/// CJK unified ideographs (+ Extension A) — the scripts unicode61 glues into
/// run-length tokens. Kana/Hangul share the failure mode but are out of
/// scope until someone needs them.
fn is_cjk(c: char) -> bool {
    matches!(c, '\u{3400}'..='\u{4DBF}' | '\u{4E00}'..='\u{9FFF}')
}

/// Whether the surface being searched carries a trigram shadow index for
/// CJK terms.
///
/// `pages` has one (`pages_fts_cjk`). `observations` deliberately does not:
/// measured on a 670 MB production deployment, a trigram shadow over the
/// 114 M-char raw log would cost ~190–340 MB (pages: 7.6 M chars → 2.2 MB
/// unicode61 vs 12.7 MB trigram, a 5.6× ratio) to speed up a path that only
/// runs when compiled wiki pages miss entirely. Its CJK terms take the LIKE
/// leg instead, which rides `idx_observations_project_created` and measured
/// 48 ms scoped to one project (1.0 s unscoped).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CjkIndex {
    /// A trigram shadow exists: CJK terms of ≥3 chars use it.
    Trigram,
    /// No trigram shadow: every CJK term goes to the LIKE leg.
    None,
}

/// Split a free-text query into routed legs. Terms without CJK go to the
/// unicode61 leg (through [`prepare_fts5_query`]'s token pipeline);
/// CJK-bearing terms go to the trigram leg when ≥3 chars and the surface
/// has one ([`CjkIndex::Trigram`]), else to the LIKE fallback. A query with
/// explicit FTS5 syntax (quotes/parens/operators) is not term-split: it runs
/// verbatim on the unicode leg and, when it contains CJK and a trigram
/// shadow exists, on the trigram leg too (both share the FTS5 grammar).
#[must_use]
pub fn route_fts_query(raw: &str, cjk_index: CjkIndex) -> RoutedFtsQuery {
    let explicit_syntax = raw.contains('"')
        || raw.contains('(')
        || raw.contains(')')
        || raw
            .split_whitespace()
            .any(|t| matches!(t, "OR" | "AND" | "NOT" | "NEAR"));
    if explicit_syntax {
        let prepared = prepare_fts5_query(raw);
        return RoutedFtsQuery {
            trigram: if has_cjk(raw) && cjk_index == CjkIndex::Trigram {
                prepared.clone()
            } else {
                String::new()
            },
            unicode: prepared,
            like_terms: Vec::new(),
        };
    }

    let mut unicode_terms: Vec<String> = Vec::new();
    let mut trigram_terms: Vec<String> = Vec::new();
    let mut like_terms: Vec<String> = Vec::new();
    for term in raw.split_whitespace() {
        if has_cjk(term) {
            if cjk_index == CjkIndex::Trigram && term.chars().count() >= 3 {
                trigram_terms.push(quote_fts5_token(term));
            } else {
                like_terms.push(term.to_string());
            }
        } else {
            unicode_terms.extend(prepare_fts5_token(term));
        }
    }
    RoutedFtsQuery {
        unicode: unicode_terms.join(" OR "),
        trigram: trigram_terms.join(" OR "),
        like_terms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colon_is_not_column_syntax() {
        // Bare multi-word → OR-joined (no explicit operator present).
        // `ui-refresh` expands to BOTH the whole token (matches content) and
        // the sub-token phrase (matches the split path index) — see
        // `quote_fts5_token`.
        let q = prepare_fts5_query("pick: handoff ui-refresh");
        assert_eq!(
            q,
            "\"pick\" OR handoff OR (\"ui-refresh\" OR \"ui refresh\")"
        );
    }

    #[test]
    fn bare_multi_word_is_or_joined() {
        // The recall fix: every word no longer has to co-occur.
        assert_eq!(
            prepare_fts5_query("cross project search strategy"),
            "cross OR project OR search OR strategy"
        );
    }

    #[test]
    fn portuguese_accented_terms_or_join_and_keep_accents() {
        // PT natural-language query: tokens preserved (accents intact),
        // joined with OR so a page matching any term is found.
        assert_eq!(
            prepare_fts5_query("descrição testes commits"),
            "descrição OR testes OR commits"
        );
    }

    #[test]
    fn single_word_has_no_or() {
        assert_eq!(prepare_fts5_query("handoff"), "handoff");
    }

    /// Regression: a filename like `current.md` used to pass through bare and
    /// FTS5 errored with `syntax error near "."`. Quoting it as a phrase both
    /// avoids the error and matches `architecture-current.md` (the tokens
    /// `current` + `md` are adjacent in the indexed path).
    #[test]
    fn dotted_filename_token_is_quoted() {
        // Whole token OR sub-token phrase. The split form (`current md`)
        // matches the tokenised path; the whole form covers content tokens.
        assert_eq!(
            prepare_fts5_query("current.md"),
            "(\"current.md\" OR \"current md\")"
        );
        assert_eq!(
            prepare_fts5_query("00-index.md"),
            "(\"00-index.md\" OR \"00 index md\")"
        );
        assert_eq!(
            prepare_fts5_query("a/b/c.md"),
            "(\"a/b/c.md\" OR \"a b c md\")"
        );
    }

    /// Regression for the live-found bug: searching `ui-refresh` returned
    /// nothing even though `follow-ups/ui-refresh-scroll-restoration.md`
    /// exists. The old quoting produced `"ui-refresh"`, which FTS5 does NOT
    /// match against the indexed `ui refresh`; the sub-token phrase
    /// `"ui refresh"` does. See the real-FTS5 test in `ops.rs`.
    #[test]
    fn hyphenated_token_quotes_as_subtoken_phrase() {
        assert_eq!(
            prepare_fts5_query("ui-refresh"),
            "(\"ui-refresh\" OR \"ui refresh\")"
        );
        assert_eq!(
            prepare_fts5_query("scroll-restoration"),
            "(\"scroll-restoration\" OR \"scroll restoration\")"
        );
    }

    /// The FTS5 prefix operator (`term*`) must survive — a trailing `*` is not
    /// quoted away.
    #[test]
    fn prefix_star_token_stays_bare() {
        assert_eq!(prepare_fts5_query("curr*"), "curr*");
    }

    #[test]
    fn empty_yields_empty() {
        assert_eq!(prepare_fts5_query("   "), "");
    }

    #[test]
    fn quote_emits_whole_and_subtoken_phrase() {
        // Punctuated identifier → both forms OR'd.
        assert_eq!(
            quote_fts5_token("ui-refresh"),
            r#"("ui-refresh" OR "ui refresh")"#
        );
        // A literal-quote fragment keeps the simple escaped form (no expansion).
        assert_eq!(quote_fts5_token(r#"say "hello""#), r#""say ""hello""""#);
        // No punctuation → single phrase.
        assert_eq!(quote_fts5_token("handoff"), r#""handoff""#);
    }

    #[test]
    fn boolean_operators_are_preserved() {
        assert_eq!(prepare_fts5_query("quick OR slow"), "quick OR slow");
    }

    /// AND is the FTS5 default but operators can be explicit — when the
    /// caller writes one, the OR-join must NOT mangle it into
    /// `foo OR AND OR bar`. Same for NOT and NEAR. (The escape hatch from
    /// the broad-recall default is what makes the OR-join safe to land.)
    #[test]
    fn explicit_and_operator_is_preserved() {
        assert_eq!(prepare_fts5_query("foo AND bar"), "foo AND bar");
    }

    #[test]
    fn explicit_not_operator_is_preserved() {
        assert_eq!(prepare_fts5_query("foo NOT bar"), "foo NOT bar");
    }

    #[test]
    fn explicit_near_operator_is_preserved() {
        assert_eq!(prepare_fts5_query("foo NEAR bar"), "foo NEAR bar");
    }

    /// A query containing a quoted phrase is treated as explicit FTS5
    /// syntax — `"exact phrase" baz` must not become
    /// `"exact" OR "phrase" OR baz` (which destroys the phrase semantics).
    /// The exact assertion is "space-joined, not OR-joined"; what the
    /// individual tokens look like after `prepare_fts5_token` is a
    /// separate concern (and unchanged from pre-#58 behaviour).
    #[test]
    fn quoted_phrase_query_is_not_or_joined() {
        let q = prepare_fts5_query("\"exact phrase\" baz");
        assert!(
            !q.contains(" OR "),
            "explicit quoted-phrase query must not get OR-joined; got {q}"
        );
    }

    /// Same escape-hatch logic for parenthesised sub-expressions —
    /// `(foo OR bar) AND baz` must survive unmangled.
    #[test]
    fn parenthesised_query_is_not_or_joined() {
        let q = prepare_fts5_query("(foo OR bar) AND baz");
        assert!(
            !q.contains("OR (foo"),
            "parens detection must skip OR-join entirely; got {q}"
        );
        assert!(
            q.contains("AND"),
            "explicit AND inside parens query must survive; got {q}"
        );
    }

    #[test]
    fn known_columns_are_preserved() {
        assert_eq!(prepare_fts5_query("title:handoff"), "title:handoff");
    }

    /// Two-character CJK terms — the most common Chinese query shape — can
    /// never match the trigram index, so they must route to the LIKE leg.
    #[test]
    fn route_short_cjk_terms_to_like() {
        let r = route_fts_query("记忆 迁移", CjkIndex::Trigram);
        assert_eq!(r.unicode, "");
        assert_eq!(r.trigram, "");
        assert_eq!(r.like_terms, vec!["记忆", "迁移"]);
    }

    #[test]
    fn route_long_cjk_terms_to_trigram_as_phrases() {
        let r = route_fts_query("反向工程 商业秘密", CjkIndex::Trigram);
        assert_eq!(r.unicode, "");
        assert_eq!(r.trigram, "\"反向工程\" OR \"商业秘密\"");
        assert!(r.like_terms.is_empty());
    }

    #[test]
    fn route_mixed_query_splits_legs_per_term() {
        let r = route_fts_query("zotero 导入 工作流", CjkIndex::Trigram);
        assert_eq!(r.unicode, "zotero");
        assert_eq!(r.trigram, "\"工作流\"");
        assert_eq!(r.like_terms, vec!["导入"]);
    }

    #[test]
    fn route_pure_ascii_matches_prepare() {
        let r = route_fts_query("cross project search", CjkIndex::Trigram);
        assert_eq!(r.unicode, prepare_fts5_query("cross project search"));
        assert_eq!(r.trigram, "");
        assert!(r.like_terms.is_empty());
    }

    /// Explicit FTS5 syntax is never term-split; with CJK present the same
    /// prepared string runs on both FTS legs (shared query grammar).
    #[test]
    fn route_explicit_syntax_with_cjk_runs_both_fts_legs() {
        let r = route_fts_query("\"反向工程\" AND zotero", CjkIndex::Trigram);
        assert_eq!(r.unicode, r.trigram);
        assert!(!r.unicode.is_empty());
        assert!(r.like_terms.is_empty());
    }

    #[test]
    fn route_empty_query_is_empty() {
        assert!(route_fts_query("   ", CjkIndex::Trigram).is_empty());
        assert!(route_fts_query("   ", CjkIndex::None).is_empty());
    }

    /// Surfaces without a trigram shadow (the raw observation log) send
    /// EVERY CJK term to the LIKE leg — including ≥3-char terms that would
    /// otherwise be a trigram MATCH — while Latin terms keep unicode61.
    #[test]
    fn route_without_trigram_sends_all_cjk_to_like() {
        let r = route_fts_query("zotero 导入 反向工程", CjkIndex::None);
        assert_eq!(r.unicode, "zotero");
        assert_eq!(r.trigram, "");
        assert_eq!(r.like_terms, vec!["导入", "反向工程"]);
    }

    /// Explicit FTS5 syntax with CJK: without a trigram shadow there is no
    /// second FTS leg to run it on.
    #[test]
    fn route_without_trigram_explicit_syntax_has_no_trigram_leg() {
        let r = route_fts_query("\"反向工程\" AND zotero", CjkIndex::None);
        assert!(!r.unicode.is_empty());
        assert_eq!(r.trigram, "");
    }
}
