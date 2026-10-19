//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Token-comparison helpers shared across the auth surfaces (session proxy,
//! collaboration room, session shares).

/// Constant-time comparison of two token strings: equal iff the lengths and
/// every byte match. The length difference is folded into the same
/// accumulator as the byte walk (no early exit on a length mismatch), so the
/// runtime does not depend on where the first differing byte is — or on
/// whether the lengths differ at all. Mirrors `secrets.compare_digest`
/// semantics; the old per-module copies short-circuited on length (ANT-344,
/// item 3).
///
/// Under the v1 "viewers trust the server operator" premise (board decision
/// D1, ANT-310) timing is an accepted-risk class, but every collab and
/// session-proxy comparison now goes through this one helper so the premise
/// is documented in exactly one place.
pub(crate) fn tokens_equal(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    let mut diff: u8 = (a.len() ^ b.len()) as u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// `Option`-shaped constant-time token equality, preserving the exact
/// semantics of the `Option<&str> == Option<&str>` compares it replaces
/// (including `None == None`) — ANT-344, item 3.
pub(crate) fn opt_tokens_equal(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => tokens_equal(a, b),
        (None, None) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_strings_match() {
        assert!(tokens_equal("abcdef", "abcdef"));
        assert!(tokens_equal("", ""));
    }

    #[test]
    fn differing_bytes_do_not_match() {
        assert!(!tokens_equal("abcdef", "abcdez"));
        assert!(!tokens_equal("abcdef", "abxdef"));
    }

    #[test]
    fn differing_lengths_do_not_match() {
        assert!(!tokens_equal("abcdef", "abcde"));
        assert!(!tokens_equal("", "a"));
    }

    #[test]
    fn opt_shape_preserves_option_equality() {
        assert!(opt_tokens_equal(Some("t"), Some("t")));
        assert!(!opt_tokens_equal(Some("t"), Some("u")));
        assert!(!opt_tokens_equal(Some("t"), None));
        assert!(!opt_tokens_equal(None, Some("t")));
        assert!(opt_tokens_equal(None, None));
    }
}
