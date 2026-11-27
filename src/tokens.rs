//! Token estimation.
//!
//! ContextKit does not ship a model tokenizer, so every count it reports is an estimate.
//! The heuristic below (UTF-8 bytes / 4, rounded up) is a common approximation for
//! English text and source code with BPE tokenizers. Real counts vary by model.

use serde::Serialize;

/// Bytes per token assumed by the estimate.
pub const BYTES_PER_TOKEN: usize = 4;

/// Human-readable description of the estimation method.
pub const METHOD: &str =
    "estimate: ceil(utf8_bytes / 4); actual counts depend on the model's tokenizer";

/// Estimated token count of `text`.
pub fn estimate(text: &str) -> usize {
    text.len().div_ceil(BYTES_PER_TOKEN)
}

/// A token count that always states whether it is exact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct TokenCount {
    pub value: usize,
    /// Always `false` today: ContextKit has no tokenizer.
    pub exact: bool,
    pub method: &'static str,
}

impl TokenCount {
    pub fn estimated(value: usize) -> Self {
        Self {
            value,
            exact: false,
            method: METHOD,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimates_round_up() {
        assert_eq!(estimate(""), 0);
        assert_eq!(estimate("abc"), 1);
        assert_eq!(estimate("abcde"), 2);
        assert!(!TokenCount::estimated(3).exact);
    }
}
