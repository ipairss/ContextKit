//! Content-based detection of credentials.
//!
//! File-name rules (see `index::walk::is_sensitive`) catch `.env` files and key files, but
//! not a token pasted into ordinary source. This module looks for high-confidence secret
//! formats in file contents. Only formats with distinctive prefixes or structure are
//! included, to keep false positives rare; generic "password = ..." heuristics are not.

use std::sync::OnceLock;

use regex::Regex;

/// Secret formats: (description, pattern).
const PATTERNS: &[(&str, &str)] = &[
    (
        "private key",
        r"-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY(?: BLOCK)?-----",
    ),
    ("AWS access key id", r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b"),
    ("GitHub token", r"\bgh[pousr]_[A-Za-z0-9]{36,}\b"),
    (
        "GitHub fine-grained token",
        r"\bgithub_pat_[A-Za-z0-9_]{60,}\b",
    ),
    ("GitLab token", r"\bglpat-[A-Za-z0-9_-]{20,}\b"),
    ("Slack token", r"\bxox[abposr]-[A-Za-z0-9-]{10,}"),
    ("Stripe live key", r"\b[rs]k_live_[A-Za-z0-9]{20,}\b"),
    ("Google API key", r"\bAIza[0-9A-Za-z_-]{35}\b"),
    ("Anthropic API key", r"\bsk-ant-[A-Za-z0-9_-]{40,}"),
    (
        "OpenAI API key",
        r"\bsk-(?:proj|svcacct|admin)-[A-Za-z0-9_-]{40,}",
    ),
    ("npm token", r"\bnpm_[A-Za-z0-9]{36}\b"),
    ("PyPI token", r"\bpypi-AgEIcHlwaS5vcmc[A-Za-z0-9_-]{50,}"),
    ("Hugging Face token", r"\bhf_[A-Za-z0-9]{34,}\b"),
    (
        "Discord bot token",
        r"\b[MN][A-Za-z0-9_-]{23,25}\.[A-Za-z0-9_-]{6}\.[A-Za-z0-9_-]{27,}\b",
    ),
    ("Roblox security cookie", r"_\|WARNING:-DO-NOT-SHARE-THIS\."),
];

fn compiled() -> &'static [(&'static str, Regex)] {
    static COMPILED: OnceLock<Vec<(&'static str, Regex)>> = OnceLock::new();
    COMPILED.get_or_init(|| {
        PATTERNS
            .iter()
            .filter_map(|(name, pattern)| match Regex::new(pattern) {
                Ok(re) => Some((*name, re)),
                Err(e) => {
                    // Covered by a unit test; logged rather than panicking in production.
                    tracing::error!(secret = name, error = %e, "invalid secret pattern ignored");
                    None
                }
            })
            .collect()
    })
}

/// A detected secret. The secret value itself is never stored or printed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SecretFinding {
    pub kind: &'static str,
    /// 1-based line of the first match.
    pub line: u32,
}

/// Returns the earliest secret found in `text`, if any.
pub fn find_secret(text: &str) -> Option<SecretFinding> {
    compiled()
        .iter()
        .filter_map(|(kind, re)| re.find(text).map(|m| (m.start(), *kind)))
        .min_by_key(|(start, _)| *start)
        .map(|(start, kind)| SecretFinding {
            kind,
            line: u32::try_from(text[..start].bytes().filter(|&b| b == b'\n').count() + 1)
                .unwrap_or(u32::MAX),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Test values are assembled at runtime so this file never contains a real-looking
    // secret (which would also trip push protection on hosting services).
    fn fake(prefix: &str, body_char: char, len: usize) -> String {
        format!("{prefix}{}", body_char.to_string().repeat(len))
    }

    #[test]
    fn every_pattern_compiles() {
        assert_eq!(compiled().len(), PATTERNS.len());
    }

    #[test]
    fn detects_common_formats() {
        let cases = [
            (
                format!("-----BEGIN {}PRIVATE KEY-----", "RSA "),
                "private key",
            ),
            (format!("-----BEGIN {}PRIVATE KEY-----", ""), "private key"),
            (fake("AKIA", 'Q', 16), "AWS access key id"),
            (fake("ghp_", 'a', 36), "GitHub token"),
            (fake("sk_live_", 'b', 24), "Stripe live key"),
            (fake("AIza", 'c', 35), "Google API key"),
            (fake("sk-ant-api03-", 'd', 60), "Anthropic API key"),
            (fake("npm_", 'e', 36), "npm token"),
        ];
        for (secret, kind) in cases {
            let text = format!("line one\nconst KEY = \"{secret}\";\n");
            let found = find_secret(&text);
            assert_eq!(found, Some(SecretFinding { kind, line: 2 }), "{kind}");
        }
    }

    #[test]
    fn ignores_ordinary_code() {
        let text = r#"
            let api_key = std::env::var("API_KEY")?;
            const PREFIX: &str = "ghp_";
            // -----BEGIN PUBLIC KEY-----
            let aws = "AKIA" ;
            fn sk_live_mode() {}
        "#;
        assert_eq!(find_secret(text), None);
        assert_eq!(find_secret(include_str!("secrets.rs")), None);
    }
}
