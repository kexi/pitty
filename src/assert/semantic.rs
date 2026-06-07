//! `expect_semantic` assertion: judge whether the output is "close enough" to
//! an expected `text` by a similarity score over a pluggable backend.
//!
//! v0.2 ships only a lexical-approximation backend (token-bag cosine
//! similarity). The [`SemanticBackend`] trait fixes the YAML contract now so a
//! future `--features semantic-embeddings` backend can be swapped in without a
//! grammar change.

use std::collections::HashMap;

/// A similarity backend: given two texts, return a score in `0.0..=1.0`.
///
/// The trait is the seam that keeps the scenario grammar stable across backend
/// changes. Why a trait rather than a free function: the planned embeddings
/// backend needs to own state (a model handle / API client), which a free
/// function cannot carry, and selecting it behind a Cargo feature is cleaner
/// when the variants share one interface.
pub trait SemanticBackend {
    /// Similarity of `a` and `b`, where `1.0` is identical and `0.0` is no
    /// overlap.
    fn similarity(&self, a: &str, b: &str) -> f64;
}

/// The default v0.2 backend: lexical (token-bag) approximation.
///
/// It normalizes both texts (lowercase, strip non-alphanumeric, tokenize on
/// whitespace) and returns the cosine similarity of their token-frequency
/// vectors. This rewards shared vocabulary and is order-insensitive.
///
/// Why lexical-only for v0.2 (user-approved): a true semantic backend needs
/// either a network embeddings API (cost, non-determinism, network in tests)
/// or a bundled local model (large binary, platform builds). Both are deferred
/// to a later release behind a feature flag; the lexical backend has zero new
/// dependencies and is deterministic, which suits CI. Its limitation —
/// blindness to paraphrase and synonymy — is surfaced in the failure message
/// so authors are not misled into trusting it as true semantics.
#[derive(Debug, Default, Clone, Copy)]
pub struct LexicalBackend;

impl SemanticBackend for LexicalBackend {
    fn similarity(&self, a: &str, b: &str) -> f64 {
        cosine_similarity(&token_counts(a), &token_counts(b))
    }
}

/// Outcome of an `expect_semantic` evaluation.
pub struct SemanticResult {
    /// Whether the computed score met the threshold.
    pub passed: bool,
    /// On failure, the score, threshold, and a backend caveat. `None` on pass.
    pub message: Option<String>,
}

/// Evaluate output against expected `text` at a `threshold`, using `backend`.
///
/// Passes when `score >= threshold`. The failure message includes the computed
/// score and a note that this is a lexical approximation, so an author does not
/// mistake a low score for a definitive "semantically different" verdict.
pub fn evaluate(
    backend: &dyn SemanticBackend,
    output: &str,
    expected: &str,
    threshold: f64,
) -> SemanticResult {
    let score = backend.similarity(output, expected);
    // Why a strict `>=` with no epsilon tolerance (R6): the lexical score is a
    // float, so a "clean" fraction like 1/2 is not represented exactly and a
    // threshold authored as `0.5` against a half-overlap can fall just short.
    // Adding an epsilon would make the boundary fuzzy and surprising in the other
    // direction (a score meant to be below would slip through), and the right fix
    // is for authors to leave headroom on round thresholds — which the README now
    // states. Keeping `>=` exact preserves a single, predictable comparison.
    if score >= threshold {
        return SemanticResult {
            passed: true,
            message: None,
        };
    }
    SemanticResult {
        passed: false,
        // Why include the caveat in the message: the lexical backend cannot see
        // paraphrase or synonymy, so a near-miss may be a true semantic match
        // the backend cannot detect. Naming the limitation points authors at the
        // planned embeddings backend instead of chasing a false negative.
        message: Some(format!(
            "semantic similarity {score:.3} below threshold {threshold:.3} \
             (lexical approximation; semantic embeddings backend planned for a later release)"
        )),
    }
}

/// Tokenize and count word frequencies after normalization.
///
/// Normalization lowercases and keeps only alphanumeric characters within each
/// whitespace-separated token, dropping tokens that normalize to empty (pure
/// punctuation). This makes "Failed!" and "failed" the same token.
fn token_counts(text: &str) -> HashMap<String, u32> {
    let mut counts = HashMap::new();
    for raw in text.split_whitespace() {
        let token: String = raw
            .chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(|c| c.to_lowercase())
            .collect();
        if token.is_empty() {
            continue;
        }
        *counts.entry(token).or_insert(0) += 1;
    }
    counts
}

/// Cosine similarity of two token-frequency vectors.
///
/// Returns `0.0` when either side is empty (no shared dimension is possible),
/// and `1.0` for identical bags. The dot product runs over the smaller map for
/// efficiency; magnitudes use each map's own counts.
fn cosine_similarity(a: &HashMap<String, u32>, b: &HashMap<String, u32>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let (small, large) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let mut dot = 0.0f64;
    for (token, &count) in small {
        if let Some(&other) = large.get(token) {
            dot += f64::from(count) * f64::from(other);
        }
    }
    let denom = magnitude(a) * magnitude(b);
    if denom == 0.0 {
        return 0.0;
    }
    dot / denom
}

/// The Euclidean norm of a token-frequency vector (`sqrt(sum of counts^2)`).
fn magnitude(counts: &HashMap<String, u32>) -> f64 {
    counts
        .values()
        .map(|&c| f64::from(c) * f64::from(c))
        .sum::<f64>()
        .sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score(a: &str, b: &str) -> f64 {
        LexicalBackend.similarity(a, b)
    }

    #[test]
    fn identical_text_scores_one() {
        // Identical content (modulo case/punctuation) must score 1.0.
        assert!((score("Authentication failed.", "authentication failed") - 1.0).abs() < 1e-9);
    }

    #[test]
    fn unrelated_text_scores_low() {
        // Disjoint vocabulary must produce a near-zero score.
        let s = score(
            "the quick brown fox",
            "completely different unrelated words",
        );
        assert!(s < 0.1, "expected low score, got {s}");
    }

    #[test]
    fn threshold_boundary_is_inclusive() {
        // A score exactly at the threshold must pass (>= comparison).
        // Two of three tokens shared on each side gives a known cosine; pick a
        // threshold at the computed value to exercise the boundary.
        let s = score("alpha beta gamma", "alpha beta delta");
        let at = evaluate(&LexicalBackend, "alpha beta gamma", "alpha beta delta", s);
        assert!(at.passed, "score {s} must pass at an equal threshold");
        let above = evaluate(
            &LexicalBackend,
            "alpha beta gamma",
            "alpha beta delta",
            s + 1e-6,
        );
        assert!(!above.passed, "score {s} must fail just above threshold");
    }

    #[test]
    fn normalization_ignores_case_and_punctuation() {
        // Case and surrounding punctuation must not change tokenization.
        assert!((score("ERROR: Token-Expired!", "error token-expired") - 1.0).abs() < 1e-9);
    }

    #[test]
    fn paraphrase_scores_below_one() {
        // The lexical backend is blind to synonymy: a paraphrase with no shared
        // content words scores low, documenting the known weakness that the
        // failure message warns about.
        let s = score(
            "the login attempt was rejected",
            "authentication request denied",
        );
        assert!(
            s < 0.3,
            "paraphrase should score low for lexical backend: {s}"
        );
    }

    #[test]
    fn failure_message_notes_lexical_limitation() {
        // A failing evaluation must mention the lexical approximation and the
        // planned embeddings backend so authors are not misled.
        let r = evaluate(&LexicalBackend, "apple", "orange", 0.9);
        assert!(!r.passed);
        let msg = r.message.unwrap();
        assert!(msg.contains("lexical approximation"));
        assert!(msg.contains("embeddings"));
    }

    #[test]
    fn round_threshold_with_half_overlap_fails_by_float() {
        // (R6) Known, fixed behavior: a "looks like half a match" case does NOT
        // land on a clean 0.5 cosine. One shared token out of two-vs-two gives
        // 1/2 in exact arithmetic, but the float result is just under 0.5, so an
        // author writing `similarity: 0.5` sees it fail. This pins that the
        // strict `>=` is kept (no epsilon fudge) and documents why the README
        // tells authors to leave headroom on round thresholds.
        let s = score("alpha beta", "alpha gamma");
        let r = evaluate(&LexicalBackend, "alpha beta", "alpha gamma", 0.5);
        assert!(
            !r.passed,
            "expected a 0.5 threshold to fail on the sub-0.5 float score {s}"
        );
        assert!(
            s < 0.5,
            "the half-overlap cosine must be just under 0.5: {s}"
        );
    }

    #[test]
    fn empty_input_scores_zero() {
        // An empty side has no tokens, so similarity is 0 (no shared dimension).
        assert_eq!(score("", "anything"), 0.0);
        assert_eq!(score("anything", ""), 0.0);
    }
}
