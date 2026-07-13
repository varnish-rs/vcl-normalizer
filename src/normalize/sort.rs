//! Normalize pass 5 — top-level declaration sort (`src/normalize/sort.rs`).
//!
//! After renaming (pass 4), sort `Program.decls` by `(kind_rank, canonical_json_of_decl)`.
//! Since names are canonical by that point, this is deterministic and identical
//! for two structurally-equivalent files regardless of original declaration order.

use crate::ast::{Decl, Program};

fn kind_rank(d: &Decl) -> u8 {
    match d {
        Decl::Import { .. } => 0,
        Decl::Probe { .. } => 1,
        Decl::Backend { .. } => 2,
        Decl::Acl { .. } => 3,
        Decl::Sub { .. } => 4,
    }
}

/// Sorts `p.decls` by `(kind_rank, canonical JSON of the decl)`.
pub fn run(p: &mut Program) {
    p.decls.sort_by(|a, b| {
        kind_rank(a).cmp(&kind_rank(b)).then_with(|| {
            let sa = serde_json::to_string(a).expect("Decl serialization should never fail");
            let sb = serde_json::to_string(b).expect("Decl serialization should never fail");
            sa.cmp(&sb)
        })
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::builder::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn n12_kind_rank_order() {
        let mut p = program(vec![
            sub("sub_1", vec![]),
            acl("acl_1", vec![acl_entry("1.2.3.4", Some(32), false)]),
            backend("backend_1", vec![fexpr("host", str_("example.com"))]),
            probe("probe_1", vec![fexpr("url", str_("/"))]),
            import("std"),
        ]);

        run(&mut p);

        let ranks: Vec<u8> = p.decls.iter().map(kind_rank).collect();
        assert_eq!(ranks, vec![0, 1, 2, 3, 4]);
        assert_eq!(p.decls[0].name(), "std");
        assert_eq!(p.decls[1].name(), "probe_1");
        assert_eq!(p.decls[2].name(), "backend_1");
        assert_eq!(p.decls[3].name(), "acl_1");
        assert_eq!(p.decls[4].name(), "sub_1");
    }

    #[test]
    fn n12_stable_and_deterministic_on_repeated_runs() {
        let mut p1 = program(vec![
            backend("b2", vec![fexpr("host", str_("b2.example.com"))]),
            backend("b1", vec![fexpr("host", str_("b1.example.com"))]),
            sub("vcl_recv", vec![call("vcl_hit")]),
            acl("a1", vec![acl_entry("10.0.0.0", Some(8), false)]),
        ]);
        let mut p2 = program(p1.decls.clone());

        run(&mut p1);
        run(&mut p2);

        let j1 = serde_json::to_value(&p1.decls).unwrap();
        let j2 = serde_json::to_value(&p2.decls).unwrap();
        assert_eq!(j1, j2);

        // Running again on the already-sorted program is a no-op (idempotent).
        let before = serde_json::to_value(&p1.decls).unwrap();
        run(&mut p1);
        let after = serde_json::to_value(&p1.decls).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn n12_ties_are_deterministic() {
        // Two structurally-identical ACLs: sort is a total order via JSON string
        // comparison, so any repeated run yields the same relative order.
        let mut p1 = program(vec![
            acl("z1", vec![acl_entry("1.2.3.4", Some(32), false)]),
            acl("a1", vec![acl_entry("1.2.3.4", Some(32), false)]),
        ]);
        let mut p2 = program(p1.decls.clone());

        run(&mut p1);
        run(&mut p2);

        let j1 = serde_json::to_value(&p1.decls).unwrap();
        let j2 = serde_json::to_value(&p2.decls).unwrap();
        assert_eq!(j1, j2);
    }
}
