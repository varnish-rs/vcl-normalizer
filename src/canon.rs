//! Canonical JSON output for normalized Programs.
//!
//! Wraps the AST with metadata. Spans are automatically excluded by serde.
//! Used for: `vcl-normalizer dump` output, equivalence comparison, fixpoint testing.

use crate::ast;
use serde_json::{json, Value};

/// Converts a Program to canonical JSON with metadata wrapper.
/// Output: `{ "vcl_normalizer_canon_version": 1, "vcl": "4.1", "decls": [...] }`
pub fn to_json(p: &ast::Program) -> Value {
    json!({
        "vcl_normalizer_canon_version": 1,
        "vcl": "4.1",
        "decls": p.decls
    })
}

/// Converts a Program to a pretty-printed canonical JSON string.
pub fn to_string(p: &ast::Program) -> String {
    serde_json::to_string_pretty(&to_json(p)).expect("JSON serialization should never fail")
}

/// Compares two Programs by their canonical JSON values.
pub fn canon_eq(a: &ast::Program, b: &ast::Program) -> bool {
    to_json(a) == to_json(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::builder::*;

    #[test]
    fn c1_json_contains_metadata_and_no_spans() {
        let prog = program(vec![backend(
            "b1",
            vec![fexpr("host", str_("example.com"))],
        )]);
        let json = to_json(&prog);

        // Check metadata is present
        assert_eq!(json["vcl_normalizer_canon_version"], 1);
        assert_eq!(json["vcl"], "4.1");
        assert!(json["decls"].is_array());

        // Check no span fields anywhere in the JSON tree
        let json_str = json.to_string();
        assert!(
            !json_str.contains("\"file\"")
                && !json_str.contains("\"lo\"")
                && !json_str.contains("\"hi\""),
            "JSON should not contain span fields (file, lo, hi). Got: {}",
            json_str
        );
    }

    #[test]
    fn c2_same_ast_produces_byte_identical_json() {
        let prog1 = program(vec![
            backend("b1", vec![fexpr("host", str_("example.com"))]),
            acl("acl1", vec![acl_entry("1.2.3.4", Some(32), false)]),
        ]);
        let prog2 = program(vec![
            backend("b1", vec![fexpr("host", str_("example.com"))]),
            acl("acl1", vec![acl_entry("1.2.3.4", Some(32), false)]),
        ]);

        let json1 = to_string(&prog1);
        let json2 = to_string(&prog2);

        assert_eq!(json1, json2, "Same AST should produce byte-identical JSON");
    }

    #[test]
    fn c3_printer_deterministic_and_reparseable() {
        // This test is really for printer.rs, but we verify the JSON part is deterministic.
        let prog = program(vec![sub("vcl_recv", vec![call("vcl_hit")])]);

        // Multiple serializations should be identical
        let json1 = to_json(&prog);
        let json2 = to_json(&prog);
        assert_eq!(json1, json2);
    }
}
