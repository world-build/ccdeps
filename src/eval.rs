//! Pratt parser/evaluator for C preprocessor `#if` expressions.
//!
//! Combined parse-and-evaluate in a single pass — no AST, returns `i64` directly.
//! Zero allocations on the evaluation path.
//!
//! Follows GCC's `expr.cc` (lines 1454-1810) operator-precedence structure.

use crate::macros::MacroTable;

/// Evaluate a `#if` expression string to an `i64`.
///
/// Undefined identifiers evaluate to `0` (per C standard).
/// `defined(X)` checks the macro table without expanding X.
pub fn evaluate(expr: &str, macros: &MacroTable) -> i64 {
    eval_inner(expr, macros, 0)
}

fn eval_inner(expr: &str, macros: &MacroTable, depth: u32) -> i64 {
    let expr = expr.trim();
    if expr.is_empty() || depth > 64 {
        return 0;
    }
    let mut p = Parser::new(expr, macros, depth);
    p.parse_expr(0)
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
            || (b == b'L'
                && self.pos + 1 < self.input.len()
                && self.input[self.pos + 1] == b'\'')
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
                    while self.pos < self.input.len()
                        && self.input[self.pos].is_ascii_hexdigit()
                    {
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
    depth: u32,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str, macros: &'a MacroTable, depth: u32) -> Self {
        let mut lexer = Lexer::new(input);
        let token = lexer.next();
        Self {
            lexer,
            token,
            macros,
            depth,
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
                // Copy name before advancing (it borrows from input, stays valid)
                let name = name;
                self.advance();

                if name == "defined" {
                    self.parse_defined()
                } else if let Some(value) = self.macros.get(name) {
                    eval_inner(value, self.macros, self.depth + 1)
                } else {
                    // Undefined identifier → 0 (per C standard)
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

        let result = if self.macros.is_defined(name) {
            1
        } else {
            0
        };
        self.advance();

        if has_paren {
            if self.token == Token::RParen {
                self.advance();
            }
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
        evaluate(expr, &MacroTable::new())
    }

    fn eval_with(expr: &str, macros: &MacroTable) -> i64 {
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
        assert_eq!(eval_with("defined(FOO)", &m), 1);
        assert_eq!(eval_with("defined(BAR)", &m), 0);
    }

    #[test]
    fn test_defined_no_paren() {
        let mut m = MacroTable::new();
        m.define("FOO", None);
        assert_eq!(eval_with("defined FOO", &m), 1);
        assert_eq!(eval_with("defined BAR", &m), 0);
    }

    // Macro expansion
    #[test]
    fn test_macro_expansion() {
        let mut m = MacroTable::new();
        m.define("VERSION", Some("3"));
        assert_eq!(eval_with("VERSION > 2", &m), 1);
        assert_eq!(eval_with("VERSION == 3", &m), 1);
    }

    #[test]
    fn test_macro_chain() {
        let mut m = MacroTable::new();
        m.define("A", Some("B"));
        m.define("B", Some("42"));
        assert_eq!(eval_with("A", &m), 42);
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
        assert_eq!(eval_with("MAJOR * 100 + MINOR >= 300", &m), 1);
    }

    #[test]
    fn test_defined_and_value() {
        let mut m = MacroTable::new();
        m.define("USE_FOO", Some("1"));
        assert_eq!(eval_with("defined(USE_FOO) && USE_FOO", &m), 1);
    }

    #[test]
    fn test_empty_expr() {
        assert_eq!(eval(""), 0);
    }

    #[test]
    fn test_wide_char() {
        assert_eq!(eval("L'A'"), 65);
    }
}
