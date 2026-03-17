//! Macro table with full expansion support.
//!
//! Supports both object-like and function-like macros, including:
//! - Argument substitution
//! - `#` (stringification) and `##` (token pasting) operators
//! - Recursive expansion with re-entrancy guard
//! - `__VA_ARGS__` and variadic macros
//! - Predefined macros: `__FILE__`, `__LINE__`, `__COUNTER__`

use std::collections::{HashMap, HashSet};

/// A macro definition — either object-like or function-like.
#[derive(Debug, Clone)]
pub enum MacroDef {
    /// `#define FOO` or `#define FOO value`
    ObjectLike {
        value: Option<String>,
    },
    /// `#define FOO(a, b) a + b`
    FunctionLike {
        params: Vec<String>,
        is_variadic: bool,
        /// Name of the variadic parameter (e.g. `"args"` for `args...`),
        /// or `None` for `...` (use `__VA_ARGS__`).
        variadic_name: Option<String>,
        body: String,
    },
}

/// Tracks defined macros for conditional evaluation and expansion.
#[derive(Debug, Clone)]
pub struct MacroTable {
    macros: HashMap<String, MacroDef>,
    counter: u32,
    /// Stack for `#pragma push_macro` / `pop_macro`.
    /// Each entry is (macro_name, saved_definition).
    macro_stack: Vec<(String, Option<MacroDef>)>,
}

impl MacroTable {
    pub fn new() -> Self {
        Self {
            macros: HashMap::new(),
            counter: 0,
            macro_stack: Vec::new(),
        }
    }

    /// Define an object-like macro, optionally with a value.
    ///
    /// `#define FOO` → `define("FOO", None)`
    /// `#define FOO 1` → `define("FOO", Some("1"))`
    pub fn define(&mut self, name: &str, value: Option<&str>) {
        self.macros.insert(
            name.to_string(),
            MacroDef::ObjectLike {
                value: value.map(|v| v.to_string()),
            },
        );
    }

    /// Define a function-like macro from a raw `#define` line body.
    ///
    /// `name` is the macro name, `params_and_body` is everything after the name
    /// starting with `(`.
    pub fn define_function_like(&mut self, name: &str, params_and_body: &str) {
        let params_and_body = params_and_body.trim();
        // Must start with '('
        let after_paren = match params_and_body.strip_prefix('(') {
            Some(s) => s,
            None => {
                // Not actually function-like, treat as object-like
                self.define(name, Some(params_and_body));
                return;
            }
        };

        // Find matching close paren
        let close = match after_paren.find(')') {
            Some(i) => i,
            None => {
                self.define(name, Some(params_and_body));
                return;
            }
        };

        let param_str = &after_paren[..close];
        let body = after_paren[close + 1..].trim().to_string();

        let mut params = Vec::new();
        let mut is_variadic = false;
        let mut variadic_name = None;

        for p in param_str.split(',') {
            let p = p.trim();
            if p.is_empty() && param_str.trim().is_empty() {
                // #define FOO() — zero params
                break;
            }
            if p == "..." {
                is_variadic = true;
            } else if p.ends_with("...") {
                // Named variadic: #define FOO(args...)
                let name_part = p.trim_end_matches("...").trim();
                variadic_name = Some(name_part.to_string());
                is_variadic = true;
            } else {
                params.push(p.to_string());
            }
        }

        self.macros.insert(
            name.to_string(),
            MacroDef::FunctionLike {
                params,
                is_variadic,
                variadic_name,
                body,
            },
        );
    }

    /// Remove a macro definition.
    pub fn undef(&mut self, name: &str) {
        self.macros.remove(name);
    }

    /// Check whether a macro is defined (for `defined(X)` operator).
    pub fn is_defined(&self, name: &str) -> bool {
        self.macros.contains_key(name)
    }

    /// Get the replacement text of an object-like macro.
    /// Returns `None` if undefined, function-like, or defined without a value.
    pub fn get(&self, name: &str) -> Option<&str> {
        match self.macros.get(name)? {
            MacroDef::ObjectLike { value } => value.as_deref(),
            MacroDef::FunctionLike { .. } => None,
        }
    }

    /// Get the macro definition (for function-like expansion).
    pub fn get_def(&self, name: &str) -> Option<&MacroDef> {
        self.macros.get(name)
    }

    /// Save the current definition of a macro onto the stack (`#pragma push_macro`).
    pub fn push_macro(&mut self, name: &str) {
        let current = self.macros.get(name).cloned();
        self.macro_stack.push((name.to_string(), current));
    }

    /// Restore a previously pushed macro definition (`#pragma pop_macro`).
    pub fn pop_macro(&mut self, name: &str) {
        // Search stack from top for matching name
        if let Some(idx) = self.macro_stack.iter().rposition(|(n, _)| n == name) {
            let (_, saved) = self.macro_stack.remove(idx);
            match saved {
                Some(def) => { self.macros.insert(name.to_string(), def); }
                None => { self.macros.remove(name); }
            }
        }
    }

    /// Get and increment the `__COUNTER__` value.
    pub fn next_counter(&mut self) -> u32 {
        let val = self.counter;
        self.counter += 1;
        val
    }

    /// Fully expand a token sequence, handling both object-like and function-like macros.
    ///
    /// `disabled` tracks macros currently being expanded (re-entrancy guard).
    pub fn expand(&mut self, input: &str) -> String {
        let mut disabled = HashSet::new();
        self.expand_inner(input, &mut disabled, 0)
    }

    fn expand_inner(
        &mut self,
        input: &str,
        disabled: &mut HashSet<String>,
        depth: u32,
    ) -> String {
        if depth > 64 || input.is_empty() {
            return input.to_string();
        }

        let mut result = String::with_capacity(input.len());
        let bytes = input.as_bytes();
        let mut i = 0;

        while i < bytes.len() {
            // Skip whitespace
            if bytes[i].is_ascii_whitespace() {
                result.push(bytes[i] as char);
                i += 1;
                continue;
            }

            // String literals — pass through
            if bytes[i] == b'"' {
                let start = i;
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    if bytes[i] == b'\\' && i + 1 < bytes.len() {
                        i += 1; // skip escaped char
                    }
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1; // skip closing "
                }
                result.push_str(&input[start..i]);
                continue;
            }

            // Character literals — pass through
            if bytes[i] == b'\'' {
                let start = i;
                i += 1;
                while i < bytes.len() && bytes[i] != b'\'' {
                    if bytes[i] == b'\\' && i + 1 < bytes.len() {
                        i += 1;
                    }
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1;
                }
                result.push_str(&input[start..i]);
                continue;
            }

            // Identifier
            if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
                let start = i;
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_')
                {
                    i += 1;
                }
                let ident = &input[start..i];

                // `defined` is special — pass through with its argument unexpanded
                if ident == "defined" {
                    result.push_str("defined");
                    // Skip whitespace
                    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                        result.push(bytes[i] as char);
                        i += 1;
                    }
                    if i < bytes.len() && bytes[i] == b'(' {
                        // defined(MACRO)
                        result.push('(');
                        i += 1;
                        // Skip whitespace
                        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                            result.push(bytes[i] as char);
                            i += 1;
                        }
                        // Copy identifier
                        while i < bytes.len()
                            && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_')
                        {
                            result.push(bytes[i] as char);
                            i += 1;
                        }
                        // Skip whitespace and closing paren
                        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                            result.push(bytes[i] as char);
                            i += 1;
                        }
                        if i < bytes.len() && bytes[i] == b')' {
                            result.push(')');
                            i += 1;
                        }
                    } else if i < bytes.len()
                        && (bytes[i].is_ascii_alphabetic() || bytes[i] == b'_')
                    {
                        // defined MACRO (no parens)
                        let id_start = i;
                        while i < bytes.len()
                            && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_')
                        {
                            i += 1;
                        }
                        result.push_str(&input[id_start..i]);
                    }
                    continue;
                }

                // __has_include is special — pass through unexpanded
                if ident == "__has_include" || ident == "__has_include_next" {
                    result.push_str(ident);
                    // Copy everything up to and including the matching ')'
                    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                        result.push(bytes[i] as char);
                        i += 1;
                    }
                    if i < bytes.len() && bytes[i] == b'(' {
                        result.push('(');
                        i += 1;
                        let mut depth = 1;
                        while i < bytes.len() && depth > 0 {
                            if bytes[i] == b'(' {
                                depth += 1;
                            } else if bytes[i] == b')' {
                                depth -= 1;
                            }
                            result.push(bytes[i] as char);
                            i += 1;
                        }
                    }
                    continue;
                }

                // Handle __COUNTER__
                if ident == "__COUNTER__" {
                    let val = self.next_counter();
                    result.push_str(&val.to_string());
                    continue;
                }

                // Check if macro is disabled (being expanded)
                if disabled.contains(ident) {
                    result.push_str(ident);
                    continue;
                }

                // Look up macro
                let def = self.macros.get(ident).cloned();
                match def {
                    Some(MacroDef::ObjectLike { value: Some(ref val) }) => {
                        disabled.insert(ident.to_string());
                        let expanded = self.expand_inner(val, disabled, depth + 1);
                        disabled.remove(ident);
                        result.push_str(&expanded);
                    }
                    Some(MacroDef::ObjectLike { value: None }) => {
                        // Defined without value — expands to nothing in expression context,
                        // but for token expansion we keep the identifier
                        result.push_str(ident);
                    }
                    Some(MacroDef::FunctionLike {
                        ref params,
                        is_variadic,
                        ref variadic_name,
                        ref body,
                    }) => {
                        // Function-like: must have '(' following
                        let saved_i = i;
                        // Skip whitespace between name and (
                        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                            i += 1;
                        }
                        if i < bytes.len() && bytes[i] == b'(' {
                            i += 1; // skip '('
                            let raw_args = parse_macro_args(input, &mut i);
                            let params = params.clone();
                            let variadic_name = variadic_name.clone();
                            let body = body.clone();
                            // Per C standard: arguments are fully expanded before
                            // substitution, UNLESS the parameter is used with # or ##.
                            // We pre-expand all args; substitute_args uses raw_args
                            // for # and ## contexts, expanded_args for normal substitution.
                            let expanded_args: Vec<String> = raw_args
                                .iter()
                                .map(|a| self.expand_inner(a, disabled, depth + 1))
                                .collect();
                            let expanded = substitute_args(
                                &body, &params, &raw_args, &expanded_args,
                                is_variadic, variadic_name.as_deref(),
                            );
                            disabled.insert(ident.to_string());
                            let re_expanded =
                                self.expand_inner(&expanded, disabled, depth + 1);
                            disabled.remove(ident);
                            result.push_str(&re_expanded);
                        } else {
                            // No '(' — not a macro invocation, just an identifier
                            i = saved_i;
                            result.push_str(ident);
                        }
                    }
                    None => {
                        result.push_str(ident);
                    }
                }
                continue;
            }

            // Other characters — pass through
            result.push(bytes[i] as char);
            i += 1;
        }

        result
    }
}

impl Default for MacroTable {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse comma-separated macro arguments from `input` starting after the '('.
/// Advances `pos` past the closing ')'.
fn parse_macro_args(input: &str, pos: &mut usize) -> Vec<String> {
    let bytes = input.as_bytes();
    let mut args = Vec::new();
    let mut current = String::new();
    let mut paren_depth = 0;

    while *pos < bytes.len() {
        let b = bytes[*pos];
        match b {
            b'(' => {
                paren_depth += 1;
                current.push('(');
                *pos += 1;
            }
            b')' => {
                if paren_depth == 0 {
                    *pos += 1; // skip ')'
                    args.push(current.trim().to_string());
                    return args;
                }
                paren_depth -= 1;
                current.push(')');
                *pos += 1;
            }
            b',' if paren_depth == 0 => {
                args.push(current.trim().to_string());
                current = String::new();
                *pos += 1;
            }
            b'"' => {
                // String literal — pass through
                current.push('"');
                *pos += 1;
                while *pos < bytes.len() && bytes[*pos] != b'"' {
                    if bytes[*pos] == b'\\' && *pos + 1 < bytes.len() {
                        current.push(bytes[*pos] as char);
                        *pos += 1;
                    }
                    current.push(bytes[*pos] as char);
                    *pos += 1;
                }
                if *pos < bytes.len() {
                    current.push('"');
                    *pos += 1;
                }
            }
            b'\'' => {
                current.push('\'');
                *pos += 1;
                while *pos < bytes.len() && bytes[*pos] != b'\'' {
                    if bytes[*pos] == b'\\' && *pos + 1 < bytes.len() {
                        current.push(bytes[*pos] as char);
                        *pos += 1;
                    }
                    current.push(bytes[*pos] as char);
                    *pos += 1;
                }
                if *pos < bytes.len() {
                    current.push('\'');
                    *pos += 1;
                }
            }
            _ => {
                current.push(b as char);
                *pos += 1;
            }
        }
    }

    // Unterminated — push what we have
    if !current.is_empty() {
        args.push(current.trim().to_string());
    }
    args
}

/// Substitute macro arguments into a function-like macro body.
/// Handles `#` (stringification) and `##` (token pasting).
///
/// `raw_args` are the original, unexpanded arguments (used for `#` and `##`).
/// `expanded_args` are the fully macro-expanded arguments (used for normal substitution).
fn substitute_args(
    body: &str,
    params: &[String],
    raw_args: &[String],
    expanded_args: &[String],
    is_variadic: bool,
    variadic_name: Option<&str>,
) -> String {
    let bytes = body.as_bytes();
    let mut result = String::with_capacity(body.len());
    let mut i = 0;

    while i < bytes.len() {
        // ## token pasting — uses RAW (unexpanded) args
        if i + 1 < bytes.len() && bytes[i] == b'#' && bytes[i + 1] == b'#' {
            // Trim trailing whitespace from result
            let trimmed_len = result.trim_end().len();
            result.truncate(trimmed_len);
            i += 2;
            // Skip leading whitespace after ##
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            // Get the next token and paste it
            if i < bytes.len()
                && (bytes[i].is_ascii_alphabetic() || bytes[i] == b'_')
            {
                let start = i;
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_')
                {
                    i += 1;
                }
                let token = &body[start..i];
                if let Some(arg_val) = lookup_param(token, params, raw_args, is_variadic, variadic_name) {
                    result.push_str(&arg_val);
                } else {
                    result.push_str(token);
                }
            }
            continue;
        }

        // # stringification — uses RAW (unexpanded) args
        if bytes[i] == b'#' {
            i += 1;
            // Skip whitespace
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < bytes.len()
                && (bytes[i].is_ascii_alphabetic() || bytes[i] == b'_')
            {
                let start = i;
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_')
                {
                    i += 1;
                }
                let token = &body[start..i];
                if let Some(arg_val) = lookup_param(token, params, raw_args, is_variadic, variadic_name) {
                    result.push('"');
                    for ch in arg_val.chars() {
                        if ch == '"' || ch == '\\' {
                            result.push('\\');
                        }
                        result.push(ch);
                    }
                    result.push('"');
                } else {
                    result.push('#');
                    result.push_str(token);
                }
                continue;
            }
            result.push('#');
            continue;
        }

        // Identifier — check if it's a parameter. Uses EXPANDED args.
        if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
            let start = i;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_')
            {
                i += 1;
            }
            let token = &body[start..i];
            if let Some(arg_val) = lookup_param(token, params, expanded_args, is_variadic, variadic_name) {
                result.push_str(&arg_val);
            } else {
                result.push_str(token);
            }
            continue;
        }

        // String literals in macro body — pass through without substitution
        if bytes[i] == b'"' {
            result.push('"');
            i += 1;
            while i < bytes.len() && bytes[i] != b'"' {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    result.push(bytes[i] as char);
                    i += 1;
                }
                result.push(bytes[i] as char);
                i += 1;
            }
            if i < bytes.len() {
                result.push('"');
                i += 1;
            }
            continue;
        }

        result.push(bytes[i] as char);
        i += 1;
    }

    result
}

/// Look up a parameter name, returning the corresponding argument value.
fn lookup_param(
    token: &str,
    params: &[String],
    args: &[String],
    is_variadic: bool,
    variadic_name: Option<&str>,
) -> Option<String> {
    // Check __VA_ARGS__ (anonymous variadic: `#define F(a, ...)`)
    if token == "__VA_ARGS__" && is_variadic && variadic_name.is_none() {
        let start = params.len();
        if start < args.len() {
            return Some(args[start..].join(", "));
        }
        return Some(String::new());
    }

    // Check named variadic param (e.g., `#define F(a, args...)`)
    if let Some(vname) = variadic_name {
        if token == vname {
            let start = params.len();
            if start < args.len() {
                return Some(args[start..].join(", "));
            }
            return Some(String::new());
        }
    }

    // Check regular named params
    for (idx, param) in params.iter().enumerate() {
        if param == token {
            return Some(args.get(idx).cloned().unwrap_or_default());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_define_and_lookup() {
        let mut t = MacroTable::new();
        t.define("FOO", Some("1"));
        assert!(t.is_defined("FOO"));
        assert_eq!(t.get("FOO"), Some("1"));
    }

    #[test]
    fn test_define_no_value() {
        let mut t = MacroTable::new();
        t.define("GUARD", None);
        assert!(t.is_defined("GUARD"));
        assert_eq!(t.get("GUARD"), None);
    }

    #[test]
    fn test_undef() {
        let mut t = MacroTable::new();
        t.define("FOO", Some("1"));
        t.undef("FOO");
        assert!(!t.is_defined("FOO"));
        assert_eq!(t.get("FOO"), None);
    }

    #[test]
    fn test_undefined_macro() {
        let t = MacroTable::new();
        assert!(!t.is_defined("NOPE"));
        assert_eq!(t.get("NOPE"), None);
    }

    #[test]
    fn test_redefine() {
        let mut t = MacroTable::new();
        t.define("X", Some("1"));
        t.define("X", Some("2"));
        assert_eq!(t.get("X"), Some("2"));
    }

    #[test]
    fn test_function_like_basic() {
        let mut t = MacroTable::new();
        t.define_function_like("ADD", "(a, b) a + b");
        let result = t.expand("ADD(1, 2)");
        assert_eq!(result, "1 + 2");
    }

    #[test]
    fn test_function_like_nested_parens() {
        let mut t = MacroTable::new();
        t.define_function_like("F", "(x) x");
        let result = t.expand("F((1 + 2))");
        assert_eq!(result, "(1 + 2)");
    }

    #[test]
    fn test_function_like_no_args() {
        let mut t = MacroTable::new();
        t.define_function_like("ZERO", "() 0");
        let result = t.expand("ZERO()");
        assert_eq!(result, "0");
    }

    #[test]
    fn test_stringification() {
        let mut t = MacroTable::new();
        t.define_function_like("STR", "(x) #x");
        let result = t.expand("STR(hello)");
        assert_eq!(result, "\"hello\"");
    }

    #[test]
    fn test_token_paste() {
        let mut t = MacroTable::new();
        t.define_function_like("PASTE", "(a, b) a ## b");
        let result = t.expand("PASTE(foo, bar)");
        assert_eq!(result, "foobar");
    }

    #[test]
    fn test_variadic_macro() {
        let mut t = MacroTable::new();
        t.define_function_like("LOG", "(fmt, ...) printf(fmt, __VA_ARGS__)");
        let result = t.expand("LOG(\"x=%d\", x)");
        assert_eq!(result, "printf(\"x=%d\", x)");
    }

    #[test]
    fn test_object_like_expand() {
        let mut t = MacroTable::new();
        t.define("A", Some("B"));
        t.define("B", Some("42"));
        let result = t.expand("A");
        assert_eq!(result, "42");
    }

    #[test]
    fn test_recursive_guard() {
        let mut t = MacroTable::new();
        t.define("A", Some("A")); // self-referential
        let result = t.expand("A");
        assert_eq!(result, "A"); // should not infinite loop
    }

    #[test]
    fn test_counter() {
        let mut t = MacroTable::new();
        assert_eq!(t.expand("__COUNTER__"), "0");
        assert_eq!(t.expand("__COUNTER__"), "1");
        assert_eq!(t.expand("__COUNTER__"), "2");
    }

    #[test]
    fn test_function_like_not_invoked() {
        let mut t = MacroTable::new();
        t.define_function_like("F", "(x) x + 1");
        // F without parens should not expand
        let result = t.expand("F");
        assert_eq!(result, "F");
    }

    #[test]
    fn test_expand_computed_include() {
        let mut t = MacroTable::new();
        t.define("HEADER", Some("\"foo.h\""));
        let result = t.expand("HEADER");
        assert_eq!(result, "\"foo.h\"");
    }

    #[test]
    fn test_stringize_for_include() {
        let mut t = MacroTable::new();
        t.define_function_like("STRINGIZE", "(text) STRINGIZE_I(text)");
        t.define_function_like("STRINGIZE_I", "(text) #text");
        t.define("INC", Some("pp-record.h"));
        let result = t.expand("STRINGIZE(INC)");
        // Should produce "pp-record.h" (a string literal containing the filename)
        assert_eq!(result, "\"pp-record.h\"", "stringize chain should produce quoted string");
    }

    #[test]
    fn test_expand_concat_include() {
        let mut t = MacroTable::new();
        t.define_function_like("MAKE_HEADER", "(name) #name \".h\"");
        let result = t.expand("MAKE_HEADER(foo)");
        assert_eq!(result, "\"foo\" \".h\"");
    }
}
