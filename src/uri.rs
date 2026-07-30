// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! `file://` URI ↔ filesystem path conversion, with RFC 3986 percent-coding.
//!
//! The LSP specification requires `DocumentUri` to be RFC 3986 conformant, so
//! a spec-compliant client percent-encodes everything outside the unreserved
//! set. Without a decode step, a workspace path holding a space or a non-ASCII
//! character arrives as the literal escape — `My%20Notes` rather than
//! `My Notes` — which matches no file on disk, so the document is never found,
//! never indexed, and every feature keyed on it silently serves nothing
//! (issue 069). Without an encode step the same characters go back out raw, and
//! a `#` or `?` in a file name truncates the URI at the client's parser.
//!
//! These two functions are the whole boundary: a URI is decoded exactly once on
//! the way in — every store, set, and cache downstream is keyed by the decoded
//! path — and a path is encoded exactly once on the way out, for every URI the
//! server emits (publishes, workspace edits, document links).
//!
//! The coding is hand-rolled rather than taken from `percent-encoding`: it is
//! two short byte loops with no configuration surface, and hand-rolling is what
//! lets an undecodable URI **fail open** (see [`uri_to_path`]) instead of being
//! lossily replaced with U+FFFD.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Convert an LSP URI to a filesystem path, percent-decoding the path
/// component.
///
/// Both spellings of a local file URI resolve to the same path: the empty
/// authority (`file:///path`) and the named-localhost authority
/// (`file://localhost/path`), which RFC 8089 defines as equivalent. A URI with
/// any other scheme — or none at all — is taken verbatim as a path and left
/// **undecoded**, preserving the tolerance the server has always had for a
/// client that sends a bare path.
///
/// **Malformed percent sequences fail open.** A `%` not followed by two hex
/// digits is passed through literally, and a decode whose bytes are not valid
/// UTF-8 yields the original undecoded string rather than an error: a URI the
/// server cannot decode is still more useful as a best-effort path than as a
/// dropped notification, and the caller has no channel to report a parse
/// failure on anyway.
pub fn uri_to_path(uri: &str) -> PathBuf {
    let Some(after_scheme) = uri.strip_prefix("file://") else {
        return PathBuf::from(uri);
    };
    // `file://localhost/x` → `/x`. Unambiguous: the alternative reading (the
    // relative path `localhost/x`) cannot arise here, since every path the
    // server emits is absolute and so always spells the empty authority.
    let encoded = after_scheme
        .strip_prefix("localhost")
        .filter(|rest| rest.starts_with('/'))
        .unwrap_or(after_scheme);
    PathBuf::from(percent_decode(encoded))
}

/// Convert a filesystem path to an LSP URI string, percent-encoding it per RFC
/// 3986's path rules.
///
/// `/` is kept — it is the path separator, not data — as are the unreserved
/// characters (`ALPHA` / `DIGIT` / `-` / `.` / `_` / `~`). Every other byte is
/// `%XX`-encoded: the space and the non-ASCII UTF-8 bytes that a strict client
/// rejects, and the reserved characters (`#`, `?`) that would otherwise be
/// re-read as a fragment or a query and silently truncate the path.
///
/// The inverse of [`uri_to_path`] for any absolute UTF-8 path: encoding then
/// decoding returns the original.
pub fn path_to_uri(path: &Path) -> String {
    format!("file://{}", percent_encode_path(&path.to_string_lossy()))
}

/// Decode every `%XX` sequence in `encoded`, passing malformed input through
/// unchanged (see [`uri_to_path`]).
///
/// Decoding is per byte, so a multi-byte character spelled as consecutive
/// sequences (`%E6%97%A5`) reassembles correctly; the UTF-8 validity of the
/// whole result is checked once at the end.
fn percent_decode(encoded: &str) -> String {
    let bytes = encoded.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let decoded = match (bytes.get(i), bytes.get(i + 1), bytes.get(i + 2)) {
            (Some(&b'%'), Some(&hi), Some(&lo)) => hex_digit(hi)
                .zip(hex_digit(lo))
                .map(|(hi, lo)| (hi << 4) | lo),
            _ => None,
        };
        if let Some(byte) = decoded {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    // Fail open: bytes that do not spell UTF-8 leave the input untouched rather
    // than becoming replacement characters.
    String::from_utf8(out).unwrap_or_else(|_| encoded.to_string())
}

/// Percent-encode `path` for the path component of a `file://` URI: `/` and the
/// RFC 3986 unreserved set pass through, every other byte becomes `%XX`.
fn percent_encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for &byte in path.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(char::from(byte));
        } else {
            // Uppercase hex is RFC 3986's preferred spelling for the triplet.
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// The value of one hexadecimal digit, or `None` for any other byte.
const fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{path_to_uri, uri_to_path};

    /// Every path a round-trip must survive unchanged: the plain ASCII case
    /// (which must stay byte-identical to the pre-encoding behaviour), the
    /// space, the reserved characters that corrupt an unencoded URI, and the
    /// multi-byte axis — CJK, an astral-plane emoji, and a combining mark.
    const ROUND_TRIP_PATHS: &[&str] = &[
        "/home/user/project/doc.md",
        "/home/user/My Notes/a b.md",
        "/home/user/notes/issue#42.md",
        "/home/user/notes/what?.md",
        "/home/user/notes/100%.md",
        "/home/user/ノート/日本語.md",
        "/home/user/notes/🎉 party.md",
        "/home/user/notes/e\u{301}tude.md",
        "/home/user/notes/a+b&c=d.md",
        "/home/user/notes/~tilde-_.md",
    ];

    #[test]
    fn uri_to_path_extracts_path() {
        let path = uri_to_path("file:///home/user/project/doc.md");
        assert_eq!(
            path,
            PathBuf::from("/home/user/project/doc.md"),
            "should extract filesystem path from URI"
        );
    }

    #[test]
    fn path_to_uri_creates_file_uri() {
        let uri = path_to_uri(Path::new("/home/user/project/doc.md"));
        assert_eq!(
            uri, "file:///home/user/project/doc.md",
            "should create file:// URI"
        );
    }

    #[test]
    fn plain_ascii_paths_are_untouched_in_both_directions() {
        // The guard for the no-change case: a path of unreserved characters
        // encodes to itself, so every existing client and test spelling stays
        // byte-identical to the pre-issue-069 behaviour.
        for path in [
            "/home/user/project/doc.md",
            "/tmp/.lattice.toml",
            "/a/b-c_d.e~f/g.md",
        ] {
            let uri = path_to_uri(Path::new(path));
            assert_eq!(
                uri,
                format!("file://{path}"),
                "a path of unreserved characters is emitted verbatim"
            );
            assert_eq!(
                uri_to_path(&uri),
                PathBuf::from(path),
                "and read back verbatim"
            );
        }
    }

    #[test]
    fn uri_to_path_decodes_escapes() {
        assert_eq!(
            uri_to_path("file:///home/user/My%20Notes/a%20b.md"),
            PathBuf::from("/home/user/My Notes/a b.md"),
            "%20 decodes to a space"
        );
        assert_eq!(
            uri_to_path("file:///home/user/notes/issue%2342.md"),
            PathBuf::from("/home/user/notes/issue#42.md"),
            "%23 decodes to the fragment delimiter"
        );
        assert_eq!(
            uri_to_path("file:///home/user/%E6%97%A5%E6%9C%AC%E8%AA%9E.md"),
            PathBuf::from("/home/user/日本語.md"),
            "consecutive sequences reassemble one multi-byte character each"
        );
        assert_eq!(
            uri_to_path("file:///home/user/%F0%9F%8E%89.md"),
            PathBuf::from("/home/user/🎉.md"),
            "a four-byte astral character reassembles across four sequences"
        );
        assert_eq!(
            uri_to_path("file:///home/user/e%CC%81tude.md"),
            PathBuf::from("/home/user/e\u{301}tude.md"),
            "a combining mark decodes as its own character, uncomposed"
        );
    }

    #[test]
    fn uri_to_path_accepts_lowercase_hex() {
        assert_eq!(
            uri_to_path("file:///home/user/%e6%97%a5.md"),
            PathBuf::from("/home/user/日.md"),
            "RFC 3986 makes the hex digits case-insensitive on decode"
        );
    }

    #[test]
    fn path_to_uri_encodes_reserved_and_non_ascii() {
        assert_eq!(
            path_to_uri(Path::new("/home/user/My Notes/a b.md")),
            "file:///home/user/My%20Notes/a%20b.md",
            "spaces are encoded"
        );
        assert_eq!(
            path_to_uri(Path::new("/home/user/notes/issue#42.md")),
            "file:///home/user/notes/issue%2342.md",
            "a `#` is encoded rather than read as a fragment"
        );
        assert_eq!(
            path_to_uri(Path::new("/home/user/notes/what?.md")),
            "file:///home/user/notes/what%3F.md",
            "a `?` is encoded rather than read as a query"
        );
        assert_eq!(
            path_to_uri(Path::new("/home/user/日.md")),
            "file:///home/user/%E6%97%A5.md",
            "non-ASCII is encoded byte by byte, in uppercase hex"
        );
        assert_eq!(
            path_to_uri(Path::new("/home/user/notes/100%.md")),
            "file:///home/user/notes/100%25.md",
            "a literal `%` is itself encoded, so decoding is unambiguous"
        );
    }

    #[test]
    fn encode_decode_round_trips() {
        for path in ROUND_TRIP_PATHS {
            let original = PathBuf::from(path);
            assert_eq!(
                uri_to_path(&path_to_uri(&original)),
                original,
                "encode then decode returns the original path"
            );
        }
    }

    #[test]
    fn localhost_authority_is_the_empty_authority() {
        assert_eq!(
            uri_to_path("file://localhost/home/user/My%20Notes/a.md"),
            PathBuf::from("/home/user/My Notes/a.md"),
            "RFC 8089's localhost authority names the same local file"
        );
        assert_eq!(
            uri_to_path("file:///localhost/a.md"),
            PathBuf::from("/localhost/a.md"),
            "a leading `localhost` path segment is not an authority"
        );
    }

    #[test]
    fn malformed_percent_sequences_pass_through_undecoded() {
        assert_eq!(
            uri_to_path("file:///home/user/100%.md"),
            PathBuf::from("/home/user/100%.md"),
            "a bare `%` is kept literally, not treated as an error"
        );
        assert_eq!(
            uri_to_path("file:///home/user/%zz.md"),
            PathBuf::from("/home/user/%zz.md"),
            "a `%` followed by non-hex is kept literally"
        );
        assert_eq!(
            uri_to_path("file:///home/user/trailing%2"),
            PathBuf::from("/home/user/trailing%2"),
            "a truncated sequence at the end is kept literally"
        );
        assert_eq!(
            uri_to_path("file:///home/user/%FF%FE.md"),
            PathBuf::from("/home/user/%FF%FE.md"),
            "bytes that do not spell UTF-8 leave the whole path undecoded"
        );
    }

    #[test]
    fn non_file_schemes_are_untouched() {
        assert_eq!(
            uri_to_path("untitled:Untitled-1"),
            PathBuf::from("untitled:Untitled-1"),
            "a non-file scheme is taken verbatim, exactly as before"
        );
    }

    /// Property: encoding a path and decoding the result returns the path, for
    /// any absolute UTF-8 path — including the space, reserved, multi-byte, and
    /// already-percent-looking characters that motivate the coding at all.
    #[allow(
        clippy::wildcard_imports,
        reason = "proptest's prelude is its conventional import"
    )]
    mod uri_props {
        use std::path::PathBuf;

        use proptest::prelude::*;

        use super::super::{path_to_uri, uri_to_path};

        /// Absolute paths whose segments mix ASCII, the reserved and
        /// sub-delimiter characters, 2/3/4-byte characters, and a combining
        /// mark.
        fn absolute_path() -> impl Strategy<Value = PathBuf> {
            let segment_char = prop_oneof![
                (b'a'..=b'z').prop_map(char::from),
                (b'0'..=b'9').prop_map(char::from),
                Just(' '),
                Just('#'),
                Just('?'),
                Just('%'),
                Just('&'),
                Just('+'),
                Just('-'),
                Just('.'),
                Just('~'),
                Just('é'),
                Just('日'),
                Just('🎉'),
                Just('\u{301}'),
            ];
            let segment = proptest::collection::vec(segment_char, 1..8)
                .prop_map(|cs| cs.into_iter().collect::<String>());
            proptest::collection::vec(segment, 1..5)
                .prop_map(|segments| PathBuf::from(format!("/{}", segments.join("/"))))
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(512))]

            #[test]
            fn path_uri_round_trips(path in absolute_path()) {
                prop_assert_eq!(uri_to_path(&path_to_uri(&path)), path);
            }
        }
    }
}
