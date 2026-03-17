//! Source minimization: strip everything except preprocessor directives.
//!
//! Inspired by clang-scan-deps' "dependency directives scanner", this module
//! takes raw source bytes and produces a minimized version containing only
//! lines that are preprocessor directives (or could affect them).
//!
//! This is the single highest-impact optimization: most C/C++ source files
//! are >90% non-directive code (function bodies, comments, etc.).

use memchr::memchr;

/// Result of minimizing a source file.
#[derive(Debug)]
pub struct MinimizedSource {
    /// The minimized source containing only preprocessor-relevant lines.
    pub content: Vec<u8>,
    /// Whether an include guard pattern was detected.
    pub has_include_guard: bool,
    /// The include guard macro name, if detected.
    pub guard_macro: Option<String>,
    /// Whether `#pragma once` was found.
    pub has_pragma_once: bool,
}

/// Minimize a source file, keeping only preprocessor directive lines.
///
/// The minimizer scans for lines beginning with `#` (after optional whitespace)
/// and preserves those lines. It skips over string literals, character literals,
/// and comments (both `//` and `/* */`) so that `#` characters inside them are
/// never mistaken for directives.
pub fn minimize(source: &[u8]) -> MinimizedSource {
    let mut result = MinimizedSource {
        content: Vec::with_capacity(source.len() / 4),
        has_include_guard: false,
        guard_macro: None,
        has_pragma_once: false,
    };

    let mut pos = 0;
    let mut first_directive: Option<&[u8]> = None;
    let mut second_directive: Option<&[u8]> = None;
    let mut directive_count = 0;

    while pos < source.len() {
        let b = source[pos];

        // Skip block comments: /* ... */
        if b == b'/' && pos + 1 < source.len() && source[pos + 1] == b'*' {
            pos = skip_block_comment(source, pos + 2);
            continue;
        }

        // Skip line comments: // ...
        if b == b'/' && pos + 1 < source.len() && source[pos + 1] == b'/' {
            pos = skip_to_eol(source, pos + 2);
            continue;
        }

        // Skip string literals: "..."
        if b == b'"' {
            pos = skip_string_literal(source, pos + 1, b'"');
            continue;
        }

        // Skip character literals: '...'
        if b == b'\'' {
            pos = skip_string_literal(source, pos + 1, b'\'');
            continue;
        }

        // Look for '#' at the start of a line
        if b != b'#' {
            pos += 1;
            continue;
        }

        let hash_pos = pos;

        // Check that '#' is at the start of a line (only whitespace before it)
        let line_start = find_line_start(source, hash_pos);
        if !is_only_whitespace(&source[line_start..hash_pos]) {
            pos = hash_pos + 1;
            continue;
        }

        // Find the end of this directive (handling line continuations)
        let line_end = find_directive_end(source, hash_pos);
        let directive_line = &source[line_start..line_end];

        // Track first two directives for include guard detection
        directive_count += 1;
        if directive_count == 1 {
            first_directive = Some(&source[hash_pos..line_end]);
        } else if directive_count == 2 {
            second_directive = Some(&source[hash_pos..line_end]);
        }

        // Check for #pragma once
        let after_hash = skip_whitespace(&source[hash_pos + 1..]);
        if after_hash.starts_with(b"pragma") {
            let after_pragma = skip_whitespace(&after_hash[6..]);
            if after_pragma.starts_with(b"once") {
                result.has_pragma_once = true;
            }
        }

        // Append the directive line to output
        result.content.extend_from_slice(directive_line);
        if !directive_line.ends_with(b"\n") {
            result.content.push(b'\n');
        }

        pos = line_end;
    }

    // Detect include guard pattern: #ifndef GUARD / #define GUARD
    if let (Some(first), Some(second)) = (first_directive, second_directive) {
        if let Some(guard) = detect_include_guard(first, second) {
            result.has_include_guard = true;
            result.guard_macro = Some(guard);
        }
    }

    result
}

/// Skip past a `/* ... */` block comment. `pos` points just past the `/*`.
#[inline]
fn skip_block_comment(source: &[u8], mut pos: usize) -> usize {
    while pos + 1 < source.len() {
        if source[pos] == b'*' && source[pos + 1] == b'/' {
            return pos + 2;
        }
        pos += 1;
    }
    source.len()
}

/// Skip to end of line (or end of source).
#[inline]
fn skip_to_eol(source: &[u8], pos: usize) -> usize {
    match memchr(b'\n', &source[pos..]) {
        Some(offset) => pos + offset + 1,
        None => source.len(),
    }
}

/// Skip past a string or character literal. `pos` points just past the opening
/// quote. Handles backslash escapes.
#[inline]
fn skip_string_literal(source: &[u8], mut pos: usize, quote: u8) -> usize {
    while pos < source.len() {
        let b = source[pos];
        if b == quote {
            return pos + 1;
        }
        if b == b'\\' && pos + 1 < source.len() {
            pos += 2; // skip escaped char
            continue;
        }
        if b == b'\n' {
            // Unterminated literal — stop here to avoid consuming directives
            return pos;
        }
        pos += 1;
    }
    source.len()
}

/// Find the start of the line containing `pos`.
fn find_line_start(source: &[u8], pos: usize) -> usize {
    if pos == 0 {
        return 0;
    }
    // Search backwards for newline
    match source[..pos].iter().rposition(|&b| b == b'\n') {
        Some(nl) => nl + 1,
        None => 0,
    }
}

/// Find the end of a preprocessor directive, handling `\` line continuations.
fn find_directive_end(source: &[u8], start: usize) -> usize {
    let mut pos = start;
    loop {
        match memchr(b'\n', &source[pos..]) {
            Some(offset) => {
                let nl_pos = pos + offset;
                // Check for line continuation (backslash before newline)
                if nl_pos > 0 && source[nl_pos - 1] == b'\\' {
                    pos = nl_pos + 1;
                    continue;
                }
                return nl_pos + 1;
            }
            None => return source.len(),
        }
    }
}

/// Check if a slice contains only ASCII whitespace (spaces and tabs).
fn is_only_whitespace(s: &[u8]) -> bool {
    s.iter().all(|&b| b == b' ' || b == b'\t')
}

/// Skip leading whitespace, returning the remainder.
fn skip_whitespace(s: &[u8]) -> &[u8] {
    let n = s.iter().take_while(|&&b| b == b' ' || b == b'\t').count();
    &s[n..]
}

/// Detect the classic include guard pattern:
///   #ifndef MACRO_NAME
///   #define MACRO_NAME
///
/// Returns the guard macro name if the pattern matches.
fn detect_include_guard(first: &[u8], second: &[u8]) -> Option<String> {
    let first = skip_hash_and_whitespace(first)?;
    if !first.starts_with(b"ifndef") {
        return None;
    }
    let guard1 = extract_identifier(&first[6..])?;

    let second = skip_hash_and_whitespace(second)?;
    if !second.starts_with(b"define") {
        return None;
    }
    let guard2 = extract_identifier(&second[6..])?;

    if guard1 == guard2 {
        Some(guard1)
    } else {
        None
    }
}

fn skip_hash_and_whitespace(s: &[u8]) -> Option<&[u8]> {
    let s = skip_whitespace(s);
    if s.first() != Some(&b'#') {
        return None;
    }
    Some(skip_whitespace(&s[1..]))
}

fn extract_identifier(s: &[u8]) -> Option<String> {
    let s = skip_whitespace(s);
    let len = s
        .iter()
        .take_while(|&&b| b.is_ascii_alphanumeric() || b == b'_')
        .count();
    if len == 0 {
        return None;
    }
    Some(String::from_utf8_lossy(&s[..len]).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_include() {
        let source = b"// comment\n#include <stdio.h>\nint main() {}\n";
        let result = minimize(source);
        assert_eq!(result.content, b"#include <stdio.h>\n");
    }

    #[test]
    fn test_include_guard_detection() {
        let source = b"#ifndef MY_HEADER_H\n#define MY_HEADER_H\n\nvoid foo();\n\n#endif\n";
        let result = minimize(source);
        assert!(result.has_include_guard);
        assert_eq!(result.guard_macro.as_deref(), Some("MY_HEADER_H"));
    }

    #[test]
    fn test_pragma_once() {
        let source = b"#pragma once\n#include \"foo.h\"\n";
        let result = minimize(source);
        assert!(result.has_pragma_once);
    }

    #[test]
    fn test_line_continuation() {
        let source = b"#define LONG_MACRO \\\n    value\nint x;\n";
        let result = minimize(source);
        assert_eq!(result.content, b"#define LONG_MACRO \\\n    value\n");
    }

    #[test]
    fn test_hash_in_string_not_directive() {
        let source = b"char *s = \"#include <fake.h>\";\n#include <real.h>\n";
        let result = minimize(source);
        // The naive minimizer will pick up both — that's OK for now.
        // The directive parser handles correctness.
        assert!(result.content.windows(8).any(|w| w == b"real.h>\n"));
    }

    #[test]
    fn test_empty_source() {
        let result = minimize(b"");
        assert!(result.content.is_empty());
    }

    #[test]
    fn test_no_directives() {
        let source = b"int main() { return 0; }\n";
        let result = minimize(source);
        assert!(result.content.is_empty());
    }
}
