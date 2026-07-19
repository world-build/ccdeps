//! Pratt parser/evaluator for C preprocessor `#if` expressions.
//!
//! Combined parse-and-evaluate in a single pass, no AST, returns `i64` directly.
//! Zero allocations on the evaluation path.
//!
//! Follows GCC's `expr.cc` (lines 1454-1810) operator-precedence structure.

use std::borrow::Cow;
use std::path::Path;

use crate::macros::{MacroState, MacroTable};
use crate::resolve::SearchPaths;
use crate::vfs::Vfs;

/// Context for evaluating `__has_include` in `#if` expressions.
pub struct EvalContext<'a> {
    pub search_paths: &'a SearchPaths,
    pub current_file_dir: &'a Path,
    pub vfs: &'a dyn Vfs,
}

/// Evaluate a `#if` expression string to an `i64`.
///
/// Undefined identifiers evaluate to `0` (per C standard).
/// `defined(X)` checks the macro table without expanding X.
/// Function-like macros are expanded before evaluation.
pub fn evaluate(expr: &str, macros: &mut MacroTable) -> i64 {
    eval_inner(expr, macros, 0, None)
}

/// Evaluate a `#if` expression with context for `__has_include`.
pub fn evaluate_with_context(expr: &str, macros: &mut MacroTable, ctx: &EvalContext<'_>) -> i64 {
    eval_inner(expr, macros, 0, Some(ctx))
}

/// Tri-state truth value of a conditional under incomplete macro knowledge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cond {
    True,
    False,
    Unknown,
}

/// Evaluate a `#if`/`#elif` controlling expression.
/// `conservative = false` reproduces today's semantics exactly
/// (result != 0 → True, else False; never Unknown).
pub fn evaluate_cond(
    expr: &str,
    macros: &mut MacroTable,
    ctx: &EvalContext<'_>,
    conservative: bool,
) -> Cond {
    if !conservative {
        return if evaluate_with_context(expr, macros, ctx) != 0 {
            Cond::True
        } else {
            Cond::False
        };
    }

    let expr = expr.trim();
    if expr.is_empty() {
        return Cond::False;
    }

    // Each step borrows the previous form and allocates only when it actually
    // rewrites something. A plain predef test (`#if defined(_WIN32)`,
    // `#if __GNUC__ >= 4`) rewrites nothing before it resolves to Unknown, so
    // the common conservative conditional makes zero heap allocations.

    // 1. `__has_include(...)`: found → 1; not found → Unknown, since the header
    //    may live in compiler-internal search paths we can't see.
    let expr: Cow<str> = match scan_has_include(expr, Some(ctx), Some(macros), true) {
        HasIncludeScan::NotFound => return Cond::Unknown,
        HasIncludeScan::Unchanged => Cow::Borrowed(expr),
        HasIncludeScan::Replaced(s) => Cow::Owned(s),
    };

    // 2. Replace `defined(X)` / `defined X` before expansion; an Unknown
    //    operand poisons the condition. Check for that first (allocation-free)
    //    so the poisoned path never builds a throwaway string.
    let expr = match replace_defined_cow(expr, macros) {
        Some(e) => e,
        None => return Cond::Unknown,
    };

    // 3. Expand macros, but only when the expression contains one that expands.
    let expanded: Cow<str> = if has_expandable_macro(&expr, macros) {
        Cow::Owned(macros.expand(&expr))
    } else {
        expr
    };

    // Expansion can reveal further `__has_include` / `defined` operators.
    let expanded: Cow<str> = match scan_has_include(&expanded, Some(ctx), Some(macros), true) {
        HasIncludeScan::NotFound => return Cond::Unknown,
        HasIncludeScan::Unchanged => expanded,
        HasIncludeScan::Replaced(s) => Cow::Owned(s),
    };
    let expanded = match replace_defined_cow(expanded, macros) {
        Some(e) => e,
        None => return Cond::Unknown,
    };

    // 4. Any identifier that survives expansion with Unknown state poisons
    //    the result. (Defined identifiers can legitimately remain, e.g.
    //    self-referential macros, and evaluate through the parser as today;
    //    Undefined identifiers evaluate to 0 per C.)
    if has_unknown_identifier(&expanded, macros) {
        return Cond::Unknown;
    }

    // 5. Fully decidable, run the Pratt parser as usual.
    let mut p = Parser::new(&expanded, macros);
    if p.parse_expr(0) != 0 {
        Cond::True
    } else {
        Cond::False
    }
}

/// `replace_defined`, but borrowing: returns the input unchanged (no allocation)
/// when it has no `defined` operator, `None` when an operand is Unknown, and an
/// owned rewrite only when there is decidable `defined(...)` to substitute.
fn replace_defined_cow<'a>(expr: Cow<'a, str>, macros: &MacroTable) -> Option<Cow<'a, str>> {
    if !expr.contains("defined") {
        return Some(expr);
    }
    if defined_has_unknown(&expr, macros) {
        return None;
    }
    Some(Cow::Owned(replace_defined(&expr, macros)?))
}

/// Whether any `defined(X)` / `defined X` operand is Unknown. Scans without
/// building a result, so the poisoned conditional stays allocation-free.
fn defined_has_unknown(expr: &str, macros: &MacroTable) -> bool {
    let bytes = expr.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'"' || b == b'\'' {
            i = skip_literal(bytes, i);
            continue;
        }
        if b.is_ascii_digit() {
            i = skip_pp_number(bytes, i);
            continue;
        }
        if b.is_ascii_alphabetic() || b == b'_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            if &expr[start..i] != "defined" {
                continue;
            }
            let mut j = i;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'(' {
                j += 1;
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
            }
            if j >= bytes.len() || (!bytes[j].is_ascii_alphabetic() && bytes[j] != b'_') {
                continue;
            }
            let name_start = j;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            if macros.state_of(&expr[name_start..j], true) == MacroState::Unknown {
                return true;
            }
            i = j;
            continue;
        }
        i += 1;
    }
    false
}

/// Whether the expression contains an identifier that macro expansion would
/// rewrite. Lets the evaluator skip `MacroTable::expand` (and its allocation)
/// for the common conditional whose identifiers are all predefs / undefined.
fn has_expandable_macro(expr: &str, macros: &MacroTable) -> bool {
    let bytes = expr.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'"' || b == b'\'' {
            i = skip_literal(bytes, i);
            continue;
        }
        if b.is_ascii_digit() {
            i = skip_pp_number(bytes, i);
            continue;
        }
        if b.is_ascii_alphabetic() || b == b'_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let ident = &expr[start..i];
            if ident == "defined" || ident == "__has_include" || ident == "__has_include_next" {
                continue; // operators, not expandable operands
            }
            if ident == "__COUNTER__" || macros.get_def(ident).is_some() {
                return true;
            }
            continue;
        }
        i += 1;
    }
    false
}

/// Replace `defined(X)` / `defined X` occurrences with `1`/`0` per
/// [`MacroTable::state_of`] (conservative mode). Uses the same tokenization
/// discipline as the expander: string/char literals and pp-numbers are
/// passed through untouched.
///
/// Returns `None` if any tested name is Unknown.
fn replace_defined(expr: &str, macros: &MacroTable) -> Option<String> {
    if !expr.contains("defined") {
        return Some(expr.to_string());
    }

    let bytes = expr.as_bytes();
    let mut result = String::with_capacity(expr.len());
    let mut i = 0;

    while i < bytes.len() {
        let b = bytes[i];

        // String / character literals, pass through.
        if b == b'"' || b == b'\'' {
            let start = i;
            i = skip_literal(bytes, i);
            result.push_str(&expr[start..i]);
            continue;
        }

        // pp-numbers (0x1fU, 1L, ...) start with a digit, pass through whole.
        if b.is_ascii_digit() {
            let start = i;
            i = skip_pp_number(bytes, i);
            result.push_str(&expr[start..i]);
            continue;
        }

        // Identifier
        if b.is_ascii_alphabetic() || b == b'_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let ident = &expr[start..i];
            if ident != "defined" {
                result.push_str(ident);
                continue;
            }

            // Parse `defined(NAME)` or `defined NAME`.
            let mut j = i;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            let has_paren = j < bytes.len() && bytes[j] == b'(';
            if has_paren {
                j += 1;
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
            }
            if j >= bytes.len() || (!bytes[j].is_ascii_alphabetic() && bytes[j] != b'_') {
                // Malformed `defined`, pass through; the parser yields 0 as today.
                result.push_str(ident);
                continue;
            }
            let name_start = j;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            let name = &expr[name_start..j];
            if has_paren {
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b')' {
                    j += 1;
                }
            }
            match macros.state_of(name, true) {
                MacroState::Defined => result.push('1'),
                MacroState::Undefined => result.push('0'),
                MacroState::Unknown => return None,
            }
            i = j;
            continue;
        }

        result.push(b as char);
        i += 1;
    }

    Some(result)
}

/// Whether any identifier remaining after macro expansion has Unknown state.
fn has_unknown_identifier(expr: &str, macros: &MacroTable) -> bool {
    let bytes = expr.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        let b = bytes[i];

        if b == b'"' || b == b'\'' {
            i = skip_literal(bytes, i);
            continue;
        }

        if b.is_ascii_digit() {
            i = skip_pp_number(bytes, i);
            continue;
        }

        if b.is_ascii_alphabetic() || b == b'_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            if macros.state_of(&expr[start..i], true) == MacroState::Unknown {
                return true;
            }
            continue;
        }

        i += 1;
    }

    false
}

/// Skip a string or character literal starting at the opening quote.
fn skip_literal(bytes: &[u8], mut i: usize) -> usize {
    let quote = bytes[i];
    i += 1;
    while i < bytes.len() && bytes[i] != quote {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            i += 1;
        }
        i += 1;
    }
    if i < bytes.len() {
        i += 1; // closing quote
    }
    i
}

/// Skip a pp-number token starting at a digit (covers `0x1fU`, `1L`, `1.5f`).
fn skip_pp_number(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len()
        && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'.')
    {
        i += 1;
    }
    i
}

fn eval_inner(
    expr: &str,
    macros: &mut MacroTable,
    depth: u32,
    ctx: Option<&EvalContext<'_>>,
) -> i64 {
    let expr = expr.trim();
    if expr.is_empty() || depth > 64 {
        return 0;
    }
    // Evaluate __has_include expressions first (before macro expansion).
    // Pass macros so macro arguments inside __has_include() can be expanded.
    let expr_str = resolve_has_include(expr, ctx, Some(macros));
    let expr_ref = expr_str.as_deref().unwrap_or(expr);
    // Expand macros (including function-like) before evaluating
    let expanded = macros.expand(expr_ref);
    let expanded = expanded.trim();
    // If nothing changed and it's a simple re-entrant case, evaluate directly
    if expanded == expr_ref {
        let mut p = Parser::new(expr_ref, macros);
        return p.parse_expr(0);
    }
    // Re-evaluate the expanded form (may contain further __has_include after expansion)
    let expanded2 = resolve_has_include(expanded, ctx, Some(macros));
    let final_expr = expanded2.as_deref().unwrap_or(expanded);
    let mut p = Parser::new(final_expr, macros);
    p.parse_expr(0)
}

/// Find and replace `__has_include("path")` / `__has_include(<path>)` with `0` or `1`.
/// If the argument is a macro, expands it first.
fn resolve_has_include(
    expr: &str,
    ctx: Option<&EvalContext<'_>>,
    macros: Option<&mut MacroTable>,
) -> Option<String> {
    match scan_has_include(expr, ctx, macros, false) {
        HasIncludeScan::Replaced(s) => Some(s),
        _ => None,
    }
}

/// Result of scanning an expression for `__has_include` operators.
enum HasIncludeScan {
    /// No `__has_include` present (or nothing replaceable).
    Unchanged,
    /// All occurrences resolved and replaced with `0`/`1`.
    Replaced(String),
    /// An occurrence did not resolve (`unknown_on_missing` mode only).
    NotFound,
}

/// Scan for `__has_include` / `__has_include_next` operators, replacing each
/// with `1` (found) or `0` (not found). With `unknown_on_missing`, an
/// unresolved header aborts the scan with [`HasIncludeScan::NotFound`]
/// instead of substituting `0`.
fn scan_has_include(
    expr: &str,
    ctx: Option<&EvalContext<'_>>,
    mut macros: Option<&mut MacroTable>,
    unknown_on_missing: bool,
) -> HasIncludeScan {
    if !expr.contains("__has_include") {
        return HasIncludeScan::Unchanged;
    }

    let mut result = String::with_capacity(expr.len());
    let bytes = expr.as_bytes();
    let mut i = 0;
    let mut changed = false;

    while i < bytes.len() {
        // Look for __has_include or __has_include_next
        if i + 15 <= bytes.len() && expr[i..].starts_with("__has_include") {
            let keyword_end = if expr[i..].starts_with("__has_include_next") {
                i + 18
            } else {
                i + 13
            };
            let mut j = keyword_end;
            // Skip whitespace
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'(' {
                j += 1;
                // Skip whitespace
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                // Parse "path" or <path>
                if j < bytes.len() && (bytes[j] == b'"' || bytes[j] == b'<') {
                    let delim = bytes[j];
                    let close_delim = if delim == b'"' { b'"' } else { b'>' };
                    j += 1;
                    let path_start = j;
                    while j < bytes.len() && bytes[j] != close_delim {
                        j += 1;
                    }
                    if j < bytes.len() {
                        let path = &expr[path_start..j];
                        j += 1; // skip close delimiter
                                // Skip whitespace and closing paren
                        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                            j += 1;
                        }
                        if j < bytes.len() && bytes[j] == b')' {
                            j += 1;
                            // Resolve the include
                            let found = if let Some(ctx) = ctx {
                                if delim == b'<' {
                                    ctx.search_paths.resolve_angle(path, ctx.vfs).is_some()
                                } else {
                                    ctx.search_paths
                                        .resolve_quoted(path, ctx.current_file_dir, None, ctx.vfs)
                                        .is_some()
                                }
                            } else {
                                false
                            };
                            if !found && unknown_on_missing {
                                return HasIncludeScan::NotFound;
                            }
                            result.push_str(if found { "1" } else { "0" });
                            i = j;
                            changed = true;
                            continue;
                        }
                    }
                } else if let Some(ref mut macros) = macros {
                    // Argument is not a string/angle literal, try macro expansion.
                    // Extract the full argument up to matching ')'.
                    let arg_start = j;
                    let mut depth = 1;
                    while j < bytes.len() && depth > 0 {
                        if bytes[j] == b'(' {
                            depth += 1;
                        } else if bytes[j] == b')' {
                            depth -= 1;
                        }
                        if depth > 0 {
                            j += 1;
                        }
                    }
                    let arg = &expr[arg_start..j];
                    j += 1; // skip closing ')'
                    let expanded_arg = macros.expand(arg);
                    let expanded_arg = expanded_arg.trim();
                    // Now try to parse as "path" or <path>
                    let found = if let Some(ctx) = ctx {
                        if expanded_arg.starts_with('"')
                            && expanded_arg.ends_with('"')
                            && expanded_arg.len() >= 2
                        {
                            let path = &expanded_arg[1..expanded_arg.len() - 1];
                            ctx.search_paths
                                .resolve_quoted(path, ctx.current_file_dir, None, ctx.vfs)
                                .is_some()
                        } else if expanded_arg.starts_with('<')
                            && expanded_arg.ends_with('>')
                            && expanded_arg.len() >= 2
                        {
                            let path = &expanded_arg[1..expanded_arg.len() - 1];
                            ctx.search_paths.resolve_angle(path, ctx.vfs).is_some()
                        } else {
                            false
                        }
                    } else {
                        false
                    };
                    if !found && unknown_on_missing {
                        return HasIncludeScan::NotFound;
                    }
                    result.push_str(if found { "1" } else { "0" });
                    i = j;
                    changed = true;
                    continue;
                }
            }
        }
        result.push(bytes[i] as char);
        i += 1;
    }

    if changed {
        HasIncludeScan::Replaced(result)
    } else {
        HasIncludeScan::Unchanged
    }
}

// ---------------------------------------------------------------------------
// Token
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
enum Token<'a> {
    Num(i64),
    Ident(&'a str),
    LParen,
    RParen,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Tilde,
    Bang,
    Amp,
    Pipe,
    Caret,
    AmpAmp,
    PipePipe,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Shl,
    Shr,
    Question,
    Colon,
    Eof,
}

// ---------------------------------------------------------------------------
// Lexer
// ---------------------------------------------------------------------------

struct Lexer<'a> {
    input: &'a [u8],
    pos: usize,
    src: &'a str,
}

impl<'a> Lexer<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input: input.as_bytes(),
            pos: 0,
            src: input,
        }
    }

    fn skip_ws(&mut self) {
        while self.pos < self.input.len() {
            match self.input[self.pos] {
                b' ' | b'\t' | b'\r' | b'\n' => self.pos += 1,
                _ => break,
            }
        }
    }

    fn next(&mut self) -> Token<'a> {
        self.skip_ws();
        if self.pos >= self.input.len() {
            return Token::Eof;
        }

        let b = self.input[self.pos];

        // Number
        if b.is_ascii_digit() {
            return self.lex_number();
        }

        // Character literal: 'x' or L'x'
        if b == b'\''
            || (b == b'L' && self.pos + 1 < self.input.len() && self.input[self.pos + 1] == b'\'')
        {
            return self.lex_char();
        }

        // Identifier
        if b.is_ascii_alphabetic() || b == b'_' {
            let start = self.pos;
            while self.pos < self.input.len()
                && (self.input[self.pos].is_ascii_alphanumeric() || self.input[self.pos] == b'_')
            {
                self.pos += 1;
            }
            return Token::Ident(&self.src[start..self.pos]);
        }

        self.pos += 1;
        match b {
            b'(' => Token::LParen,
            b')' => Token::RParen,
            b'+' => Token::Plus,
            b'-' => Token::Minus,
            b'*' => Token::Star,
            b'/' => Token::Slash,
            b'%' => Token::Percent,
            b'~' => Token::Tilde,
            b'^' => Token::Caret,
            b'?' => Token::Question,
            b':' => Token::Colon,
            b'!' => {
                if self.pos < self.input.len() && self.input[self.pos] == b'=' {
                    self.pos += 1;
                    Token::Ne
                } else {
                    Token::Bang
                }
            }
            b'=' => {
                if self.pos < self.input.len() && self.input[self.pos] == b'=' {
                    self.pos += 1;
                    Token::Eq
                } else {
                    Token::Eof
                }
            }
            b'<' => {
                if self.pos < self.input.len() && self.input[self.pos] == b'=' {
                    self.pos += 1;
                    Token::Le
                } else if self.pos < self.input.len() && self.input[self.pos] == b'<' {
                    self.pos += 1;
                    Token::Shl
                } else {
                    Token::Lt
                }
            }
            b'>' => {
                if self.pos < self.input.len() && self.input[self.pos] == b'=' {
                    self.pos += 1;
                    Token::Ge
                } else if self.pos < self.input.len() && self.input[self.pos] == b'>' {
                    self.pos += 1;
                    Token::Shr
                } else {
                    Token::Gt
                }
            }
            b'&' => {
                if self.pos < self.input.len() && self.input[self.pos] == b'&' {
                    self.pos += 1;
                    Token::AmpAmp
                } else {
                    Token::Amp
                }
            }
            b'|' => {
                if self.pos < self.input.len() && self.input[self.pos] == b'|' {
                    self.pos += 1;
                    Token::PipePipe
                } else {
                    Token::Pipe
                }
            }
            _ => Token::Eof,
        }
    }

    fn lex_number(&mut self) -> Token<'a> {
        let mut val: i64 = 0;

        if self.input[self.pos] == b'0' && self.pos + 1 < self.input.len() {
            match self.input[self.pos + 1] {
                b'x' | b'X' => {
                    self.pos += 2;
                    while self.pos < self.input.len() && self.input[self.pos].is_ascii_hexdigit() {
                        let d = self.input[self.pos];
                        val = val.wrapping_mul(16).wrapping_add(match d {
                            b'0'..=b'9' => (d - b'0') as i64,
                            b'a'..=b'f' => (d - b'a' + 10) as i64,
                            b'A'..=b'F' => (d - b'A' + 10) as i64,
                            _ => unreachable!(),
                        });
                        self.pos += 1;
                    }
                    self.skip_int_suffix();
                    return Token::Num(val);
                }
                b'b' | b'B' => {
                    self.pos += 2;
                    while self.pos < self.input.len()
                        && (self.input[self.pos] == b'0' || self.input[self.pos] == b'1')
                    {
                        val = val
                            .wrapping_mul(2)
                            .wrapping_add((self.input[self.pos] - b'0') as i64);
                        self.pos += 1;
                    }
                    self.skip_int_suffix();
                    return Token::Num(val);
                }
                b'0'..=b'7' => {
                    self.pos += 1;
                    while self.pos < self.input.len()
                        && self.input[self.pos] >= b'0'
                        && self.input[self.pos] <= b'7'
                    {
                        val = val
                            .wrapping_mul(8)
                            .wrapping_add((self.input[self.pos] - b'0') as i64);
                        self.pos += 1;
                    }
                    self.skip_int_suffix();
                    return Token::Num(val);
                }
                _ => {}
            }
        }

        // Decimal
        while self.pos < self.input.len() && self.input[self.pos].is_ascii_digit() {
            val = val
                .wrapping_mul(10)
                .wrapping_add((self.input[self.pos] - b'0') as i64);
            self.pos += 1;
        }
        self.skip_int_suffix();
        Token::Num(val)
    }

    fn skip_int_suffix(&mut self) {
        while self.pos < self.input.len() {
            match self.input[self.pos] {
                b'u' | b'U' | b'l' | b'L' => self.pos += 1,
                _ => break,
            }
        }
    }

    fn lex_char(&mut self) -> Token<'a> {
        // Skip L prefix
        if self.input[self.pos] == b'L' {
            self.pos += 1;
        }
        self.pos += 1; // skip opening '

        let val = if self.pos < self.input.len() && self.input[self.pos] == b'\\' {
            self.pos += 1;
            if self.pos >= self.input.len() {
                0
            } else {
                let c = self.input[self.pos];
                self.pos += 1;
                match c {
                    b'n' => b'\n' as i64,
                    b't' => b'\t' as i64,
                    b'r' => b'\r' as i64,
                    b'\\' => b'\\' as i64,
                    b'\'' => b'\'' as i64,
                    b'"' => b'"' as i64,
                    b'0' => 0,
                    b'a' => 7,
                    b'b' => 8,
                    b'f' => 12,
                    b'v' => 11,
                    b'x' => {
                        let mut v: i64 = 0;
                        while self.pos < self.input.len()
                            && self.input[self.pos].is_ascii_hexdigit()
                        {
                            let d = self.input[self.pos];
                            v = v * 16
                                + match d {
                                    b'0'..=b'9' => (d - b'0') as i64,
                                    b'a'..=b'f' => (d - b'a' + 10) as i64,
                                    b'A'..=b'F' => (d - b'A' + 10) as i64,
                                    _ => unreachable!(),
                                };
                            self.pos += 1;
                        }
                        v
                    }
                    _ => c as i64,
                }
            }
        } else if self.pos < self.input.len() {
            let c = self.input[self.pos] as i64;
            self.pos += 1;
            c
        } else {
            0
        };

        // Skip closing '
        if self.pos < self.input.len() && self.input[self.pos] == b'\'' {
            self.pos += 1;
        }
        Token::Num(val)
    }
}

// ---------------------------------------------------------------------------
// Parser (Pratt)
// ---------------------------------------------------------------------------

/// Binding power for prefix (unary) operators.
const PREFIX_BP: u8 = 25;

struct Parser<'a> {
    lexer: Lexer<'a>,
    token: Token<'a>,
    macros: &'a MacroTable,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str, macros: &'a MacroTable) -> Self {
        let mut lexer = Lexer::new(input);
        let token = lexer.next();
        Self {
            lexer,
            token,
            macros,
        }
    }

    fn advance(&mut self) {
        self.token = self.lexer.next();
    }

    fn parse_expr(&mut self, min_bp: u8) -> i64 {
        // ---- nud (null denotation) ----
        let mut lhs = match self.token {
            Token::Num(n) => {
                self.advance();
                n
            }
            Token::Ident(name) => {
                self.advance();

                if name == "defined" {
                    self.parse_defined()
                } else {
                    // After macro expansion, remaining identifiers are undefined → 0 (per C standard)
                    0
                }
            }
            Token::LParen => {
                self.advance();
                let val = self.parse_expr(0);
                if self.token == Token::RParen {
                    self.advance();
                }
                val
            }
            Token::Bang => {
                self.advance();
                let val = self.parse_expr(PREFIX_BP);
                if val == 0 {
                    1
                } else {
                    0
                }
            }
            Token::Tilde => {
                self.advance();
                let val = self.parse_expr(PREFIX_BP);
                !val
            }
            Token::Minus => {
                self.advance();
                let val = self.parse_expr(PREFIX_BP);
                val.wrapping_neg()
            }
            Token::Plus => {
                self.advance();
                self.parse_expr(PREFIX_BP)
            }
            _ => 0,
        };

        // ---- led (left denotation) ----
        loop {
            // (left_bp, right_bp), left < right for left-associative
            let (lbp, rbp) = match self.token {
                Token::PipePipe => (4, 5),
                Token::AmpAmp => (6, 7),
                Token::Pipe => (8, 9),
                Token::Caret => (10, 11),
                Token::Amp => (12, 13),
                Token::Eq | Token::Ne => (14, 15),
                Token::Lt | Token::Le | Token::Gt | Token::Ge => (16, 17),
                Token::Shl | Token::Shr => (18, 19),
                Token::Plus | Token::Minus => (20, 21),
                Token::Star | Token::Slash | Token::Percent => (22, 23),
                Token::Question => (3, 0), // ternary handled specially
                _ => break,
            };

            if lbp < min_bp {
                break;
            }

            let op = self.token;
            self.advance();

            match op {
                Token::Question => {
                    // Ternary: condition ? then : else
                    let then_val = self.parse_expr(0);
                    if self.token == Token::Colon {
                        self.advance();
                    }
                    let else_val = self.parse_expr(3); // right-associative
                    lhs = if lhs != 0 { then_val } else { else_val };
                }
                _ => {
                    let rhs = self.parse_expr(rbp);
                    lhs = apply_binop(op, lhs, rhs);
                }
            }
        }

        lhs
    }

    /// Parse `defined(MACRO)` or `defined MACRO`.
    fn parse_defined(&mut self) -> i64 {
        let has_paren = self.token == Token::LParen;
        if has_paren {
            self.advance();
        }

        let name = if let Token::Ident(n) = self.token {
            n
        } else {
            return 0;
        };

        let result = if self.macros.is_defined(name) { 1 } else { 0 };
        self.advance();

        if has_paren && self.token == Token::RParen {
            self.advance();
        }

        result
    }
}

fn apply_binop(op: Token<'_>, lhs: i64, rhs: i64) -> i64 {
    match op {
        Token::Plus => lhs.wrapping_add(rhs),
        Token::Minus => lhs.wrapping_sub(rhs),
        Token::Star => lhs.wrapping_mul(rhs),
        Token::Slash => {
            if rhs != 0 {
                lhs.wrapping_div(rhs)
            } else {
                0
            }
        }
        Token::Percent => {
            if rhs != 0 {
                lhs.wrapping_rem(rhs)
            } else {
                0
            }
        }
        Token::Shl => lhs.wrapping_shl(rhs as u32),
        Token::Shr => lhs.wrapping_shr(rhs as u32),
        Token::Lt => (lhs < rhs) as i64,
        Token::Le => (lhs <= rhs) as i64,
        Token::Gt => (lhs > rhs) as i64,
        Token::Ge => (lhs >= rhs) as i64,
        Token::Eq => (lhs == rhs) as i64,
        Token::Ne => (lhs != rhs) as i64,
        Token::Amp => lhs & rhs,
        Token::Caret => lhs ^ rhs,
        Token::Pipe => lhs | rhs,
        Token::AmpAmp => ((lhs != 0) && (rhs != 0)) as i64,
        Token::PipePipe => ((lhs != 0) || (rhs != 0)) as i64,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(expr: &str) -> i64 {
        evaluate(expr, &mut MacroTable::new())
    }

    fn eval_with(expr: &str, macros: &mut MacroTable) -> i64 {
        evaluate(expr, macros)
    }

    // Integer literals
    #[test]
    fn test_decimal() {
        assert_eq!(eval("42"), 42);
    }

    #[test]
    fn test_hex() {
        assert_eq!(eval("0xFF"), 255);
        assert_eq!(eval("0x1A"), 26);
    }

    #[test]
    fn test_octal() {
        assert_eq!(eval("077"), 63);
    }

    #[test]
    fn test_binary() {
        assert_eq!(eval("0b1010"), 10);
    }

    #[test]
    fn test_suffixes() {
        assert_eq!(eval("42L"), 42);
        assert_eq!(eval("42ULL"), 42);
        assert_eq!(eval("42u"), 42);
    }

    #[test]
    fn test_zero() {
        assert_eq!(eval("0"), 0);
    }

    // Character literals
    #[test]
    fn test_char_literal() {
        assert_eq!(eval("'A'"), 65);
        assert_eq!(eval("'\\n'"), 10);
        assert_eq!(eval("'\\x41'"), 65);
        assert_eq!(eval("'\\0'"), 0);
    }

    // Arithmetic
    #[test]
    fn test_add_sub() {
        assert_eq!(eval("1 + 2"), 3);
        assert_eq!(eval("10 - 3"), 7);
    }

    #[test]
    fn test_mul_div_mod() {
        assert_eq!(eval("3 * 4"), 12);
        assert_eq!(eval("10 / 3"), 3);
        assert_eq!(eval("10 % 3"), 1);
    }

    #[test]
    fn test_div_by_zero() {
        assert_eq!(eval("1 / 0"), 0);
        assert_eq!(eval("1 % 0"), 0);
    }

    #[test]
    fn test_precedence() {
        assert_eq!(eval("2 + 3 * 4"), 14);
        assert_eq!(eval("(2 + 3) * 4"), 20);
    }

    // Unary
    #[test]
    fn test_unary_minus() {
        assert_eq!(eval("-1"), -1);
        assert_eq!(eval("-(-1)"), 1);
    }

    #[test]
    fn test_logical_not() {
        assert_eq!(eval("!0"), 1);
        assert_eq!(eval("!1"), 0);
        assert_eq!(eval("!42"), 0);
    }

    #[test]
    fn test_bitwise_not() {
        assert_eq!(eval("~0"), -1);
    }

    // Comparison
    #[test]
    fn test_comparisons() {
        assert_eq!(eval("1 < 2"), 1);
        assert_eq!(eval("2 < 1"), 0);
        assert_eq!(eval("1 <= 1"), 1);
        assert_eq!(eval("1 == 1"), 1);
        assert_eq!(eval("1 != 2"), 1);
        assert_eq!(eval("2 >= 2"), 1);
        assert_eq!(eval("3 > 2"), 1);
    }

    // Logical
    #[test]
    fn test_logical_and_or() {
        assert_eq!(eval("1 && 1"), 1);
        assert_eq!(eval("1 && 0"), 0);
        assert_eq!(eval("0 || 1"), 1);
        assert_eq!(eval("0 || 0"), 0);
    }

    // Bitwise
    #[test]
    fn test_bitwise() {
        assert_eq!(eval("0xFF & 0x0F"), 15);
        assert_eq!(eval("0xF0 | 0x0F"), 255);
        assert_eq!(eval("0xFF ^ 0x0F"), 240);
    }

    // Shifts
    #[test]
    fn test_shifts() {
        assert_eq!(eval("1 << 4"), 16);
        assert_eq!(eval("16 >> 2"), 4);
    }

    // Ternary
    #[test]
    fn test_ternary() {
        assert_eq!(eval("1 ? 10 : 20"), 10);
        assert_eq!(eval("0 ? 10 : 20"), 20);
    }

    #[test]
    fn test_nested_ternary() {
        // Right-associative: 1 ? 2 : (0 ? 3 : 4)
        assert_eq!(eval("1 ? 2 : 0 ? 3 : 4"), 2);
        assert_eq!(eval("0 ? 2 : 0 ? 3 : 4"), 4);
    }

    // defined()
    #[test]
    fn test_defined_paren() {
        let mut m = MacroTable::new();
        m.define("FOO", Some("1"));
        assert_eq!(eval_with("defined(FOO)", &mut m), 1);
        assert_eq!(eval_with("defined(BAR)", &mut m), 0);
    }

    #[test]
    fn test_defined_no_paren() {
        let mut m = MacroTable::new();
        m.define("FOO", None);
        assert_eq!(eval_with("defined FOO", &mut m), 1);
        assert_eq!(eval_with("defined BAR", &mut m), 0);
    }

    // Macro expansion
    #[test]
    fn test_macro_expansion() {
        let mut m = MacroTable::new();
        m.define("VERSION", Some("3"));
        assert_eq!(eval_with("VERSION > 2", &mut m), 1);
        assert_eq!(eval_with("VERSION == 3", &mut m), 1);
    }

    #[test]
    fn test_macro_chain() {
        let mut m = MacroTable::new();
        m.define("A", Some("B"));
        m.define("B", Some("42"));
        assert_eq!(eval_with("A", &mut m), 42);
    }

    #[test]
    fn test_undefined_identifier() {
        assert_eq!(eval("UNKNOWN"), 0);
    }

    // Complex expressions
    #[test]
    fn test_complex_expr() {
        let mut m = MacroTable::new();
        m.define("MAJOR", Some("3"));
        m.define("MINOR", Some("14"));
        assert_eq!(eval_with("MAJOR * 100 + MINOR >= 300", &mut m), 1);
    }

    #[test]
    fn test_defined_and_value() {
        let mut m = MacroTable::new();
        m.define("USE_FOO", Some("1"));
        assert_eq!(eval_with("defined(USE_FOO) && USE_FOO", &mut m), 1);
    }

    #[test]
    fn test_empty_expr() {
        assert_eq!(eval(""), 0);
    }

    #[test]
    fn test_wide_char() {
        assert_eq!(eval("L'A'"), 65);
    }

    // Function-like macros in #if
    #[test]
    fn test_function_like_macro_in_if() {
        let mut m = MacroTable::new();
        m.define_function_like("CHECK", "(x) x");
        assert_eq!(eval_with("CHECK(1)", &mut m), 1);
        assert_eq!(eval_with("CHECK(0)", &mut m), 0);
    }

    #[test]
    fn test_function_like_macro_complex() {
        let mut m = MacroTable::new();
        m.define_function_like("VER", "(maj, min) maj * 100 + min");
        assert_eq!(eval_with("VER(3, 14) >= 300", &mut m), 1);
    }

    // __has_include
    #[test]
    fn test_has_include_quoted() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("exists.h"), "").unwrap();

        let search_paths = crate::resolve::SearchPaths::default();
        let ctx = EvalContext {
            search_paths: &search_paths,
            current_file_dir: dir.path(),
            vfs: &crate::vfs::OsVfs,
        };
        let mut m = MacroTable::new();
        assert_eq!(
            evaluate_with_context("__has_include(\"exists.h\")", &mut m, &ctx),
            1
        );
        assert_eq!(
            evaluate_with_context("__has_include(\"nonexistent.h\")", &mut m, &ctx),
            0
        );
    }

    #[test]
    fn test_has_include_angle() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("sys.h"), "").unwrap();

        let search_paths = crate::resolve::SearchPaths {
            include_paths: vec![dir.path().to_path_buf()],
            ..Default::default()
        };
        let ctx = EvalContext {
            search_paths: &search_paths,
            current_file_dir: dir.path(),
            vfs: &crate::vfs::OsVfs,
        };
        let mut m = MacroTable::new();
        assert_eq!(
            evaluate_with_context("__has_include(<sys.h>)", &mut m, &ctx),
            1
        );
        assert_eq!(
            evaluate_with_context("__has_include(<missing.h>)", &mut m, &ctx),
            0
        );
    }

    // Conservative tri-state evaluation (Cow fast paths).
    fn cond(expr: &str, macros: &mut MacroTable) -> Cond {
        let sp = crate::resolve::SearchPaths::default();
        let ctx = EvalContext {
            search_paths: &sp,
            current_file_dir: Path::new("."),
            vfs: &crate::vfs::OsVfs,
        };
        evaluate_cond(expr, macros, &ctx, true)
    }

    #[test]
    fn conservative_predef_is_unknown() {
        let mut m = MacroTable::new();
        assert_eq!(cond("defined(__linux__)", &mut m), Cond::Unknown);
        assert_eq!(cond("__GNUC__ >= 4", &mut m), Cond::Unknown);
        assert_eq!(
            cond("defined(_WIN32) || defined(_WIN64)", &mut m),
            Cond::Unknown
        );
    }

    #[test]
    fn conservative_decidable() {
        let mut m = MacroTable::new();
        m.define("FOO", Some("3"));
        assert_eq!(cond("FOO > 2", &mut m), Cond::True);
        assert_eq!(cond("FOO < 2", &mut m), Cond::False);
        assert_eq!(cond("defined(FOO)", &mut m), Cond::True);
        // BAR is not underscore-prefixed, so it is known-undefined, not Unknown.
        assert_eq!(cond("defined(BAR)", &mut m), Cond::False);
        assert_eq!(cond("BAR", &mut m), Cond::False);
    }

    #[test]
    fn conservative_marked_unknown_poisons() {
        let mut m = MacroTable::new();
        m.mark_unknown("WITH_X");
        assert_eq!(cond("WITH_X", &mut m), Cond::Unknown);
        assert_eq!(cond("defined(WITH_X)", &mut m), Cond::Unknown);
    }

    #[test]
    fn test_has_include_in_expression() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("opt.h"), "").unwrap();

        let search_paths = crate::resolve::SearchPaths::default();
        let ctx = EvalContext {
            search_paths: &search_paths,
            current_file_dir: dir.path(),
            vfs: &crate::vfs::OsVfs,
        };
        let mut m = MacroTable::new();
        assert_eq!(
            evaluate_with_context("__has_include(\"opt.h\") && 1", &mut m, &ctx,),
            1
        );
        assert_eq!(
            evaluate_with_context("__has_include(\"nope.h\") || 0", &mut m, &ctx,),
            0
        );
    }
}
