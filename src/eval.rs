//! Pratt parser/evaluator for C preprocessor `#if` expressions.
//!
//! Combined parse-and-evaluate in a single pass — no AST, returns `i64` directly.
//! Zero allocations on the evaluation path.
//!
//! Follows GCC's `expr.cc` (lines 1454-1810) operator-precedence structure.

use std::path::Path;

use crate::macros::MacroTable;
use crate::resolve::SearchPaths;

/// Context for evaluating `__has_include` in `#if` expressions.
pub struct EvalContext<'a> {
    pub search_paths: &'a SearchPaths,
    pub current_file_dir: &'a Path,
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
    mut macros: Option<&mut MacroTable>,
) -> Option<String> {
    if !expr.contains("__has_include") {
        return None;
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
                                    ctx.search_paths.resolve_angle(path).is_some()
                                } else {
                                    ctx.search_paths
                                        .resolve_quoted(path, ctx.current_file_dir, None)
                                        .is_some()
                                }
                            } else {
                                false
                            };
                            result.push_str(if found { "1" } else { "0" });
                            i = j;
                            changed = true;
                            continue;
                        }
                    }
                } else if let Some(ref mut macros) = macros {
                    // Argument is not a string/angle literal — try macro expansion.
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
                                .resolve_quoted(path, ctx.current_file_dir, None)
                                .is_some()
                        } else if expanded_arg.starts_with('<')
                            && expanded_arg.ends_with('>')
                            && expanded_arg.len() >= 2
                        {
                            let path = &expanded_arg[1..expanded_arg.len() - 1];
                            ctx.search_paths.resolve_angle(path).is_some()
                        } else {
                            false
                        }
                    } else {
                        false
                    };
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
        Some(result)
    } else {
        None
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
            // (left_bp, right_bp) — left < right for left-associative
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

    #[test]
    fn test_has_include_in_expression() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("opt.h"), "").unwrap();

        let search_paths = crate::resolve::SearchPaths::default();
        let ctx = EvalContext {
            search_paths: &search_paths,
            current_file_dir: dir.path(),
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
