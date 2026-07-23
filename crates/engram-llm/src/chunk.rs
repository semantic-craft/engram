//! Markdown-aware chunking for long-document embeddings.
//!
//! Wiki pages larger than one embedding request used to be
//! head-truncated (OpenAI path) or rejected outright (Voyage 400), so
//! only the first few KB of a long page ever reached the vector index.
//! [`chunk_markdown`] splits a document into contiguous, boundary-aligned
//! slices that each fit a conservative provider input budget;
//! `Embedder::embed_document_chunked` embeds one vector per chunk.

/// Byte budget per document chunk.
///
/// ~6 KB is ≈1500 English BPE tokens, and ≈2000 CJK chars (≤ ~6000
/// tokens even at a pessimistic 3 tokens/char) — comfortably under the
/// 8192-token limit of OpenAI-compatible embedding servers and far
/// under Voyage's 16K+ context. It also sits below the 8000-byte hard
/// cap `truncate_for_embedding` applies on the OpenAI path, so chunks
/// pass through that guard unmodified.
///
/// The `engram embed` backfill relies on an exact correspondence with
/// [`chunk_markdown`]: a body of `len <= DOC_CHUNK_MAX_BYTES` produces
/// exactly one chunk, a longer body produces at least two.
pub const DOC_CHUNK_MAX_BYTES: usize = 6_000;

/// Ceiling on chunks per document (≈384 KB of indexed text). Text
/// beyond the cap is not embedded — FTS still covers it — which bounds
/// the synchronous per-write embedding cost for pathological inputs.
pub const MAX_DOC_CHUNKS: usize = 64;

/// Split a markdown document into embedding-sized chunks.
///
/// Chunks are contiguous slices of the input (concatenating them
/// reproduces a prefix of the document, the whole document unless the
/// [`MAX_DOC_CHUNKS`] cap truncated it). Split points prefer markdown
/// block boundaries — the start of a `#` heading line, or the first
/// non-blank line after blank lines — greedily packing blocks up to
/// [`DOC_CHUNK_MAX_BYTES`] per chunk. A single block larger than the
/// budget is hard-split at the last newline in the window, falling
/// back to a UTF-8 character boundary.
///
/// Text within the budget (including empty text) returns the input as
/// a single chunk, preserving the pre-chunking behaviour byte for byte.
#[must_use]
pub fn chunk_markdown(text: &str) -> Vec<&str> {
    chunk_markdown_impl(text, DOC_CHUNK_MAX_BYTES, MAX_DOC_CHUNKS)
}

fn chunk_markdown_impl(text: &str, max_bytes: usize, max_chunks: usize) -> Vec<&str> {
    if text.len() <= max_bytes {
        return vec![text];
    }
    let boundaries = block_starts(text);
    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < text.len() {
        if chunks.len() >= max_chunks {
            tracing::debug!(
                total_bytes = text.len(),
                covered_bytes = start,
                max_chunks,
                "document exceeds chunk cap; tail not embedded (FTS still covers it)"
            );
            break;
        }
        let window_end = start.saturating_add(max_bytes);
        if text.len() <= window_end {
            chunks.push(&text[start..]);
            break;
        }
        // Furthest block boundary inside (start, window_end]; without
        // one, hard-split inside the window.
        let hi = boundaries.partition_point(|&b| b <= window_end);
        let lo = boundaries.partition_point(|&b| b <= start);
        let split = if hi > lo {
            boundaries[hi - 1]
        } else {
            hard_split(text, start, window_end)
        };
        chunks.push(&text[start..split]);
        start = split;
    }
    chunks
}

/// Byte offsets where a new markdown block begins: the start of a line
/// whose first non-whitespace char is `#` (heading), or the start of
/// the first non-blank line after one or more blank lines. Offset 0 is
/// never included (a chunk already starts there).
fn block_starts(text: &str) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut offset = 0usize;
    let mut prev_blank = false;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        let is_blank = trimmed.is_empty();
        if offset > 0 && !is_blank && (prev_blank || trimmed.starts_with('#')) {
            starts.push(offset);
        }
        prev_blank = is_blank;
        offset += line.len();
    }
    starts
}

/// Split point for a block larger than the budget: after the last
/// newline within `(start, window_end]`, else the largest UTF-8
/// boundary `<= window_end`. Always returns a point strictly beyond
/// `start` (the budget is far larger than any single UTF-8 char).
fn hard_split(text: &str, start: usize, window_end: usize) -> usize {
    let mut end = window_end;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    match text[start..end].rfind('\n') {
        Some(i) if i > 0 => start + i + 1,
        _ => end,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_a_single_identical_chunk() {
        assert_eq!(chunk_markdown("hello world"), vec!["hello world"]);
        assert_eq!(chunk_markdown(""), vec![""]);
    }

    #[test]
    fn chunks_are_contiguous_and_within_budget() {
        let para = format!("{}\n\n", "内存系统的长文档分块测试。".repeat(30));
        let text = para.repeat(30); // ~9 KB * 3 of CJK
        let chunks = chunk_markdown_impl(&text, 2_000, MAX_DOC_CHUNKS);
        assert!(chunks.len() > 1);
        for c in &chunks {
            assert!(
                c.len() <= 2_000,
                "chunk of {} bytes exceeds budget",
                c.len()
            );
        }
        let rejoined: String = chunks.concat();
        assert_eq!(rejoined, text, "chunks must reproduce the document");
    }

    #[test]
    fn heading_starts_a_new_chunk() {
        let sec_a = format!("# Alpha\n{}\n", "aaaa ".repeat(100));
        let sec_b = format!("# Beta\n{}\n", "bbbb ".repeat(100));
        let text = format!("{sec_a}{sec_b}");
        // Budget fits either section alone but not both.
        let chunks = chunk_markdown_impl(&text, sec_a.len().max(sec_b.len()) + 10, 64);
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].starts_with("# Alpha"));
        assert!(chunks[1].starts_with("# Beta"));
    }

    #[test]
    fn giant_block_without_boundaries_hard_splits_on_utf8() {
        let text = "汉".repeat(3_000); // 9 KB, no newlines, no blanks
        let chunks = chunk_markdown_impl(&text, 2_000, 64);
        assert!(chunks.len() > 1);
        for c in &chunks {
            assert!(c.len() <= 2_000);
            assert!(c.chars().all(|ch| ch == '汉'), "no split mid-codepoint");
        }
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn hard_split_prefers_newline() {
        let line = format!("{}\n", "x".repeat(1_500));
        let text = line.repeat(4); // one 6 KB "block" of non-blank lines
        let chunks = chunk_markdown_impl(&text, 2_000, 64);
        assert!(chunks.len() > 1);
        for c in &chunks[..chunks.len() - 1] {
            assert!(c.ends_with('\n'), "split should land after a newline");
        }
    }

    #[test]
    fn chunk_cap_bounds_output() {
        let text = format!("{}\n\n", "word ".repeat(100)).repeat(200);
        let chunks = chunk_markdown_impl(&text, 1_000, 5);
        assert_eq!(chunks.len(), 5);
    }

    #[test]
    fn budget_boundary_is_exact() {
        let text = "a".repeat(DOC_CHUNK_MAX_BYTES);
        assert_eq!(chunk_markdown(&text).len(), 1);
        let text = "a".repeat(DOC_CHUNK_MAX_BYTES + 1);
        assert!(chunk_markdown(&text).len() > 1);
    }
}
