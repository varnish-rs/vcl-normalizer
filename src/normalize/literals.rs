//! Pass 1: literal canonicalization and symbol normalization
//!
//! Operations:
//! 1. Header lowercasing: in Expr::Var and Expr::Call target paths, when a
//!    segment == "http", lowercase the NEXT segment only. This applies only
//!    to *read* positions -- HTTP header lookups (`req.http.X` in an
//!    expression) match case-insensitively, so two files differing only in
//!    the case used to *read* a header are equivalent. `Lvalue` (the target
//!    of `set`/`unset`) is deliberately NOT touched here: writing a header
//!    is case-sensitive -- `set req.http.foO = ...;` and
//!    `set req.http.foo = ...;` produce a header with a different literal
//!    name on the wire, so they are NOT equivalent.
//! 2. ACL default masks: entries without explicit masks get /32 (IPv4) or /128 (IPv6).
//! 3. Num text canonicalization: float-containing numeric text via f64 round-trip.

use crate::ast::*;

pub fn run(p: &mut Program) {
    for decl in &mut p.decls {
        match decl {
            Decl::Backend {
                body: Some(fields), ..
            } => {
                for field in fields {
                    normalize_field_value(&mut field.value);
                }
            }
            Decl::Probe { body: fields, .. } => {
                for field in fields {
                    normalize_field_value(&mut field.value);
                }
            }
            Decl::Acl { entries, .. } => {
                for entry in entries {
                    normalize_acl_entry(entry);
                }
            }
            Decl::Sub { body, .. } => {
                for stmt in body {
                    normalize_stmt(stmt);
                }
            }
            _ => {}
        }
    }
}

fn normalize_field_value(fv: &mut FieldValue) {
    match fv {
        FieldValue::Expr(expr) => normalize_expr(expr),
        FieldValue::Probe(fields) => {
            for field in fields {
                normalize_field_value(&mut field.value);
            }
        }
        FieldValue::StringList(_) => {}
        FieldValue::ProbeRef(_) => {}
    }
}

fn normalize_stmt(stmt: &mut Stmt) {
    match stmt {
        // `lhs` (an `Lvalue`, i.e. a write target) is deliberately left
        // untouched -- see the module doc comment.
        Stmt::Set { rhs, .. } => {
            normalize_expr(rhs);
        }
        Stmt::Unset { .. } => {}
        Stmt::Call { .. } => {}
        Stmt::Return { action, .. } => {
            if let Some(ra) = action {
                for arg in &mut ra.args {
                    normalize_expr(arg);
                }
            }
        }
        Stmt::Synthetic { value, .. } => normalize_expr(value),
        Stmt::If {
            arms, else_body, ..
        } => {
            for (cond, body) in arms {
                normalize_expr(cond);
                for stmt in body {
                    normalize_stmt(stmt);
                }
            }
            if let Some(else_stmts) = else_body {
                for stmt in else_stmts {
                    normalize_stmt(stmt);
                }
            }
        }
        Stmt::New { args, .. } => {
            for arg in args {
                normalize_expr(&mut arg.value);
            }
        }
        Stmt::Expr { expr, .. } => normalize_expr(expr),
    }
}

fn normalize_expr(expr: &mut Expr) {
    match expr {
        Expr::Str(_) => {}
        Expr::Num(text) => {
            if text.contains('.') {
                // Parse as f64 and reformat
                if let Ok(v) = text.parse::<f64>() {
                    *text = format!("{}", v);
                }
            }
        }
        Expr::Duration(_) => {} // Already normalized by parser
        Expr::Bytes(_) => {}    // Already normalized by parser
        Expr::Bool(_) => {}
        Expr::Omitted => {}
        Expr::CSource(_) => {}
        Expr::Var(parts) => {
            lowercase_after_http(parts);
        }
        Expr::Call { target, args } => {
            lowercase_after_http(target);
            for arg in args {
                normalize_expr(&mut arg.value);
            }
        }
        Expr::Unary { expr, .. } => normalize_expr(expr),
        Expr::Binary { lhs, rhs, .. } => {
            normalize_expr(lhs);
            normalize_expr(rhs);
        }
    }
}

fn lowercase_after_http(parts: &mut [String]) {
    for i in 0..parts.len() - 1 {
        if parts[i] == "http" {
            parts[i + 1] = parts[i + 1].to_lowercase();
            break; // Only lowercase the first segment after http
        }
    }
}

fn normalize_acl_entry(entry: &mut AclEntry) {
    if entry.mask.is_none() {
        entry.mask = determine_mask(&entry.addr);
    }
}

fn determine_mask(addr: &str) -> Option<u8> {
    // Check if it's an IPv6 address (contains ':')
    if addr.contains(':') {
        return Some(128);
    }

    // Check if it's an IPv4 address (4 dot-separated decimal octets)
    let parts: Vec<&str> = addr.split('.').collect();
    if parts.len() == 4 {
        let all_valid = parts
            .iter()
            .all(|part| part.parse::<u32>().map(|n| n <= 255).unwrap_or(false));
        if all_valid {
            return Some(32);
        }
    }

    // Otherwise it's a DNS name, leave as None
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    // Test N1: each duration unit and bytes unit, plus integer canonicalization
    #[test]
    fn test_duration_units() {
        let units = vec![
            ("ms", 0.001),
            ("s", 1.0),
            ("m", 60.0),
            ("h", 3600.0),
            ("d", 86400.0),
            ("w", 604800.0),
            ("y", 31_557_600.0),
        ];

        for (unit, expected_secs) in units {
            let result = duration_secs(1.0, unit);
            assert_eq!(
                result,
                Some(expected_secs),
                "unit {} should convert to {}",
                unit,
                expected_secs
            );
        }
    }

    #[test]
    fn test_bytes_units() {
        let units = vec![
            ("B", 1),
            ("KB", 1 << 10),
            ("MB", 1 << 20),
            ("GB", 1 << 30),
            ("TB", 1 << 40),
        ];

        for (unit, expected_bytes) in units {
            let result = bytes_val(1.0, unit);
            assert_eq!(
                result,
                Some(expected_bytes),
                "unit {} should convert to {}",
                unit,
                expected_bytes
            );
        }
    }

    #[test]
    fn test_num_integer_canonicalization() {
        let mut prog = builder::program(vec![builder::sub(
            "test",
            vec![builder::set(&["req", "http", "x"], builder::num("5"))],
        )]);

        run(&mut prog);

        let sub = &prog.decls[0];
        if let Decl::Sub { body, .. } = sub {
            if let Stmt::Set {
                rhs: Expr::Num(text),
                ..
            } = &body[0]
            {
                // Integer without dot should remain unchanged
                assert_eq!(text, "5");
            } else {
                panic!("Expected Set statement");
            }
        }
    }

    #[test]
    fn test_num_float_canonicalization() {
        let mut prog = builder::program(vec![builder::sub(
            "test",
            vec![builder::set(&["req", "http", "x"], builder::num("1.5"))],
        )]);

        run(&mut prog);

        let sub = &prog.decls[0];
        if let Decl::Sub { body, .. } = sub {
            if let Stmt::Set {
                rhs: Expr::Num(text),
                ..
            } = &body[0]
            {
                // Float should be reformatted via f64 round-trip
                assert_eq!(text, "1.5");
            } else {
                panic!("Expected Set statement");
            }
        }
    }

    // Test N2: header lowercasing (read positions only -- see module doc).
    #[test]
    fn test_header_write_case_preserved_in_set_lvalue() {
        // Bug regression: `set req.http.foO = req.http.Bar;` must keep the
        // write target's case exactly ("foO") while still lowercasing the
        // read ("Bar" -> "bar") -- writing a header is case-sensitive
        // (determines the literal header name on the wire); reading one is
        // a case-insensitive lookup.
        let mut prog = builder::program(vec![builder::sub(
            "test",
            vec![builder::set(
                &["req", "http", "foO"],
                builder::var(&["req", "http", "Bar"]),
            )],
        )]);

        run(&mut prog);

        let sub = &prog.decls[0];
        if let Decl::Sub { body, .. } = sub {
            if let Stmt::Set {
                lhs,
                rhs: Expr::Var(rhs_parts),
                ..
            } = &body[0]
            {
                assert_eq!(
                    &lhs.parts,
                    &["req", "http", "foO"],
                    "write target case must be preserved exactly"
                );
                assert_eq!(
                    rhs_parts,
                    &["req", "http", "bar"],
                    "read case is still lowercased for comparison"
                );
            } else {
                panic!("Expected Set statement with Var rhs");
            }
        }
    }

    #[test]
    fn test_header_write_case_preserved_in_unset_lvalue() {
        let mut prog = builder::program(vec![builder::sub(
            "test",
            vec![builder::unset(&["req", "http", "X-Forwarded-For"])],
        )]);

        run(&mut prog);

        let sub = &prog.decls[0];
        if let Decl::Sub { body, .. } = sub {
            if let Stmt::Unset { lhs, .. } = &body[0] {
                assert_eq!(&lhs.parts, &["req", "http", "X-Forwarded-For"]);
            } else {
                panic!("Expected Unset statement");
            }
        }
    }

    #[test]
    fn test_header_lowercase_in_var() {
        let mut prog = builder::program(vec![builder::sub(
            "test",
            vec![builder::set(
                &["req", "http", "x"],
                builder::var(&["req", "http", "Cookie"]),
            )],
        )]);

        run(&mut prog);

        let sub = &prog.decls[0];
        if let Decl::Sub { body, .. } = sub {
            if let Stmt::Set {
                rhs: Expr::Var(parts),
                ..
            } = &body[0]
            {
                assert_eq!(parts, &["req", "http", "cookie"]);
            } else {
                panic!("Expected Set statement with Var");
            }
        }
    }

    #[test]
    fn test_header_lowercase_in_call() {
        let mut prog = builder::program(vec![builder::sub(
            "test",
            vec![builder::set(
                &["req", "http", "x"],
                builder::fcall(&["req", "http", "get_header"], vec![]),
            )],
        )]);

        run(&mut prog);

        let sub = &prog.decls[0];
        if let Decl::Sub { body, .. } = sub {
            if let Stmt::Set {
                rhs: Expr::Call { target, .. },
                ..
            } = &body[0]
            {
                assert_eq!(target, &["req", "http", "get_header"]);
            } else {
                panic!("Expected Set statement with Call");
            }
        }
    }

    #[test]
    fn test_header_lowercase_only_after_http() {
        let mut prog = builder::program(vec![builder::sub(
            "test",
            vec![builder::set(
                &["req", "http", "x"],
                builder::var(&["req", "Url"]),
            )],
        )]);

        run(&mut prog);

        let sub = &prog.decls[0];
        if let Decl::Sub { body, .. } = sub {
            if let Stmt::Set {
                rhs: Expr::Var(parts),
                ..
            } = &body[0]
            {
                // Only the segment right after 'http' is lowercased; `req.Url`
                // has no 'http' segment at all, so it's untouched.
                assert_eq!(parts, &["req", "Url"]);
            } else {
                panic!("Expected Set statement with Var rhs");
            }
        }
    }

    // Test N3: ACL default masks
    #[test]
    fn test_acl_ipv4_mask() {
        let mut prog = builder::program(vec![builder::acl(
            "test",
            vec![builder::acl_entry("192.168.1.1", None, false)],
        )]);

        run(&mut prog);

        let acl = &prog.decls[0];
        if let Decl::Acl { entries, .. } = acl {
            assert_eq!(entries[0].mask, Some(32));
        } else {
            panic!("Expected Acl declaration");
        }
    }

    #[test]
    fn test_acl_ipv6_mask() {
        let mut prog = builder::program(vec![builder::acl(
            "test",
            vec![builder::acl_entry("::1", None, false)],
        )]);

        run(&mut prog);

        let acl = &prog.decls[0];
        if let Decl::Acl { entries, .. } = acl {
            assert_eq!(entries[0].mask, Some(128));
        } else {
            panic!("Expected Acl declaration");
        }
    }

    #[test]
    fn test_acl_dns_name_no_mask() {
        let mut prog = builder::program(vec![builder::acl(
            "test",
            vec![builder::acl_entry("example.com", None, false)],
        )]);

        run(&mut prog);

        let acl = &prog.decls[0];
        if let Decl::Acl { entries, .. } = acl {
            assert_eq!(entries[0].mask, None);
        } else {
            panic!("Expected Acl declaration");
        }
    }

    #[test]
    fn test_acl_existing_mask_preserved() {
        let mut prog = builder::program(vec![builder::acl(
            "test",
            vec![builder::acl_entry("192.168.0.0", Some(16), false)],
        )]);

        run(&mut prog);

        let acl = &prog.decls[0];
        if let Decl::Acl { entries, .. } = acl {
            assert_eq!(entries[0].mask, Some(16));
        } else {
            panic!("Expected Acl declaration");
        }
    }
}
