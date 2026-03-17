//! Preprocessor directive parsing.
//!
//! Parses minimized source into structured directive representations.

/// A parsed preprocessor directive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Directive<'a> {
    /// `#include "path"` or `#include <path>`
    Include(IncludeDirective<'a>),
    /// `#include_next "path"` or `#include_next <path>` (GNU extension)
    IncludeNext(IncludeDirective<'a>),
    /// `#import "path"` or `#import <path>` (Objective-C, once-only semantics)
    Import(IncludeDirective<'a>),
    /// `#embed "path"` or `#embed <path>` (C23, binary resource embedding)
    Embed(IncludeDirective<'a>),
    /// `#define MACRO ...`
    Define { name: &'a str, value: Option<&'a str> },
    /// `#undef MACRO`
    Undef { name: &'a str },
    /// `#ifdef MACRO`
    Ifdef { name: &'a str },
    /// `#ifndef MACRO`
    Ifndef { name: &'a str },
    /// `#if EXPR`
    If { expr: &'a str },
    /// `#elif EXPR`
    Elif { expr: &'a str },
    /// `#else`
    Else,
    /// `#endif`
    Endif,
    /// `#pragma ...`
    Pragma { tokens: &'a str },
    /// `#error ...`
    Error { message: &'a str },
    /// `#warning ...`
    Warning { message: &'a str },
    /// Unknown/unrecognized directive
    Unknown { content: &'a str },
}

/// An `#include` directive with its path and style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncludeDirective<'a> {
    pub path: &'a str,
    pub style: IncludeStyle,
}

/// Whether an include uses `"..."` or `<...>` syntax.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncludeStyle {
    /// `#include "path"` — searches relative to current file first
    Quoted,
    /// `#include <path>` — searches system/include paths only
    Angle,
    /// `#include MACRO` — macro-expanded include (needs evaluation)
    Computed,
}

/// Parse a single directive line into a structured `Directive`.
///
/// The input should be a single directive line (from minimized source),
/// starting with `#`.
pub fn parse_directive(line: &str) -> Option<Directive<'_>> {
    let line = line.trim();
    let after_hash = line.strip_prefix('#')?.trim_start();

    if let Some(rest) = strip_keyword(after_hash, "include_next") {
        return Some(parse_include_next(rest));
    }
    if let Some(rest) = strip_keyword(after_hash, "include") {
        return Some(parse_include(rest));
    }
    if let Some(rest) = strip_keyword(after_hash, "import") {
        return Some(parse_import(rest));
    }
    if let Some(rest) = strip_keyword(after_hash, "embed") {
        return Some(parse_embed(rest));
    }
    if let Some(rest) = strip_keyword(after_hash, "define") {
        let rest = rest.trim_start();
        let name_end = rest
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .unwrap_or(rest.len());
        let name = &rest[..name_end];
        // Preserve whether `(` immediately follows the name (function-like)
        // vs space-then-`(` (object-like). Only trim trailing whitespace.
        let value = if name_end < rest.len() {
            let raw = &rest[name_end..];
            let trimmed = raw.trim_end();
            if trimmed.is_empty() { None } else { Some(trimmed) }
        } else {
            None
        };
        return Some(Directive::Define { name, value });
    }
    if let Some(rest) = strip_keyword(after_hash, "undef") {
        return Some(Directive::Undef {
            name: rest.trim(),
        });
    }
    if let Some(rest) = strip_keyword(after_hash, "ifndef") {
        return Some(Directive::Ifndef {
            name: rest.trim(),
        });
    }
    if let Some(rest) = strip_keyword(after_hash, "ifdef") {
        return Some(Directive::Ifdef {
            name: rest.trim(),
        });
    }
    if let Some(rest) = strip_keyword(after_hash, "if") {
        return Some(Directive::If {
            expr: rest.trim(),
        });
    }
    if let Some(rest) = strip_keyword(after_hash, "elif") {
        return Some(Directive::Elif {
            expr: rest.trim(),
        });
    }
    if after_hash.starts_with("else") {
        return Some(Directive::Else);
    }
    if after_hash.starts_with("endif") {
        return Some(Directive::Endif);
    }
    if let Some(rest) = strip_keyword(after_hash, "pragma") {
        return Some(Directive::Pragma {
            tokens: rest.trim(),
        });
    }
    if let Some(rest) = strip_keyword(after_hash, "error") {
        return Some(Directive::Error {
            message: rest.trim(),
        });
    }
    if let Some(rest) = strip_keyword(after_hash, "warning") {
        return Some(Directive::Warning {
            message: rest.trim(),
        });
    }

    Some(Directive::Unknown { content: after_hash })
}

fn parse_include(rest: &str) -> Directive<'_> {
    Directive::Include(parse_include_path(rest))
}

fn parse_include_next(rest: &str) -> Directive<'_> {
    Directive::IncludeNext(parse_include_path(rest))
}

fn parse_import(rest: &str) -> Directive<'_> {
    Directive::Import(parse_include_path(rest))
}

fn parse_embed(rest: &str) -> Directive<'_> {
    Directive::Embed(parse_include_path(rest))
}

fn parse_include_path(rest: &str) -> IncludeDirective<'_> {
    let rest = rest.trim_start();
    if let Some(path) = extract_quoted(rest, '"', '"') {
        IncludeDirective {
            path,
            style: IncludeStyle::Quoted,
        }
    } else if let Some(path) = extract_quoted(rest, '<', '>') {
        IncludeDirective {
            path,
            style: IncludeStyle::Angle,
        }
    } else {
        IncludeDirective {
            path: rest.trim(),
            style: IncludeStyle::Computed,
        }
    }
}

fn extract_quoted(s: &str, open: char, close: char) -> Option<&str> {
    let s = s.trim_start();
    if !s.starts_with(open) {
        return None;
    }
    let inner = &s[open.len_utf8()..];
    let end = inner.find(close)?;
    Some(&inner[..end])
}

fn strip_keyword<'a>(s: &'a str, keyword: &str) -> Option<&'a str> {
    if !s.starts_with(keyword) {
        return None;
    }
    let rest = &s[keyword.len()..];
    // Ensure the keyword is followed by whitespace or end-of-string
    // to avoid matching "ifdef" when looking for "if"
    if rest.is_empty() || rest.starts_with(|c: char| c.is_ascii_whitespace()) {
        Some(rest)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_include_quoted() {
        let d = parse_directive("#include \"foo.h\"").unwrap();
        assert_eq!(
            d,
            Directive::Include(IncludeDirective {
                path: "foo.h",
                style: IncludeStyle::Quoted,
            })
        );
    }

    #[test]
    fn test_include_angle() {
        let d = parse_directive("#include <stdio.h>").unwrap();
        assert_eq!(
            d,
            Directive::Include(IncludeDirective {
                path: "stdio.h",
                style: IncludeStyle::Angle,
            })
        );
    }

    #[test]
    fn test_define() {
        let d = parse_directive("#define FOO 42").unwrap();
        assert!(matches!(d, Directive::Define { name: "FOO", value: Some(" 42") }));
    }

    #[test]
    fn test_ifdef_vs_if() {
        let d1 = parse_directive("#ifdef FOO").unwrap();
        assert!(matches!(d1, Directive::Ifdef { name: "FOO" }));

        let d2 = parse_directive("#if defined(FOO)").unwrap();
        assert!(matches!(d2, Directive::If { expr: "defined(FOO)" }));
    }

    #[test]
    fn test_ifndef() {
        let d = parse_directive("#ifndef GUARD_H").unwrap();
        assert!(matches!(d, Directive::Ifndef { name: "GUARD_H" }));
    }

    #[test]
    fn test_pragma_once() {
        let d = parse_directive("#pragma once").unwrap();
        assert!(matches!(d, Directive::Pragma { tokens: "once" }));
    }
}
