//! Macro table tracking `#define` and `#undef` directives.
//!
//! Phase 1: object-like macros only. Function-like macro expansion
//! is deferred to Phase 2.

use std::collections::HashMap;

/// Tracks defined macros for conditional evaluation.
#[derive(Debug, Clone)]
pub struct MacroTable {
    macros: HashMap<String, Option<String>>,
}

impl MacroTable {
    pub fn new() -> Self {
        Self {
            macros: HashMap::new(),
        }
    }

    /// Define a macro, optionally with a value.
    ///
    /// `#define FOO` → `define("FOO", None)`
    /// `#define FOO 1` → `define("FOO", Some("1"))`
    pub fn define(&mut self, name: &str, value: Option<&str>) {
        self.macros
            .insert(name.to_string(), value.map(|v| v.to_string()));
    }

    /// Remove a macro definition.
    pub fn undef(&mut self, name: &str) {
        self.macros.remove(name);
    }

    /// Check whether a macro is defined (for `defined(X)` operator).
    pub fn is_defined(&self, name: &str) -> bool {
        self.macros.contains_key(name)
    }

    /// Get the value of a defined macro. Returns `None` if the macro
    /// is undefined or defined without a value (`#define FOO`).
    pub fn get(&self, name: &str) -> Option<&str> {
        self.macros.get(name)?.as_deref()
    }
}

impl Default for MacroTable {
    fn default() -> Self {
        Self::new()
    }
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
}
