//! Tree diff + human-readable divergence report.
//!
//! Both input `Program`s are assumed already normalized (and top-level
//! `decls` sorted) by the `normalize` passes. Comparison walks both trees
//! in lock-step by index; the moment two corresponding nodes disagree, a
//! `Divergence` is recorded and that subtree is not descended into further.
//! Sibling nodes are still compared. Collection stops once `max_reports`
//! divergences have been recorded.

use crate::ast::{self, AclEntry, Arg, Decl, Expr, Field, FieldValue, Program, Span, Stmt};
use crate::printer;

/// One reported point of divergence between the two programs.
#[derive(Debug, Clone, PartialEq)]
pub struct Divergence {
    pub path: String,
    pub span_a: Option<Span>,
    pub span_b: Option<Span>,
    pub snippet_a: String,
    pub snippet_b: String,
}

/// Compares two (normalized) programs. Empty result means equivalent.
pub fn compare(a: &Program, b: &Program, max_reports: usize) -> Vec<Divergence> {
    let mut out = Vec::new();
    let mut path: Vec<String> = Vec::new();
    compare_decl_lists(&a.decls, &b.decls, &mut path, &mut out, max_reports);
    out
}

// ─────────────────────────── helpers ───────────────────────────

fn full(out: &[Divergence], max: usize) -> bool {
    out.len() >= max
}

fn push_div(
    path: &[String],
    extra_seg: Option<&str>,
    span_a: Option<Span>,
    span_b: Option<Span>,
    snippet_a: String,
    snippet_b: String,
    out: &mut Vec<Divergence>,
) {
    let mut full_path = path.to_vec();
    if let Some(seg) = extra_seg {
        if !seg.is_empty() {
            full_path.push(seg.to_string());
        }
    }
    out.push(Divergence {
        path: full_path.join(" › "),
        span_a,
        span_b,
        snippet_a,
        snippet_b,
    });
}

fn decl_kind(d: &Decl) -> &'static str {
    match d {
        Decl::Import { .. } => "import",
        Decl::Backend { .. } => "backend",
        Decl::Probe { .. } => "probe",
        Decl::Acl { .. } => "acl",
        Decl::Sub { .. } => "sub",
    }
}

fn stmt_kind(s: &Stmt) -> &'static str {
    match s {
        Stmt::Set { .. } => "set",
        Stmt::Unset { .. } => "unset",
        Stmt::Call { .. } => "call",
        Stmt::Return { .. } => "return",
        Stmt::Synthetic { .. } => "synthetic",
        Stmt::If { .. } => "if",
        Stmt::New { .. } => "new",
        Stmt::Expr { .. } => "expr",
    }
}

fn field_value_kind(v: &FieldValue) -> &'static str {
    match v {
        FieldValue::Expr(_) => "expr",
        FieldValue::Probe(_) => "probe",
        FieldValue::ProbeRef(_) => "probe_ref",
        FieldValue::StringList(_) => "string_list",
    }
}

fn json_snippet<T: serde::Serialize>(node: &T) -> String {
    serde_json::to_string(node).unwrap_or_else(|_| "<unserializable>".to_string())
}

// ─────────────────────────── decls ───────────────────────────

fn compare_decl_lists(
    as_: &[Decl],
    bs: &[Decl],
    path: &mut Vec<String>,
    out: &mut Vec<Divergence>,
    max: usize,
) {
    let n = as_.len().max(bs.len());
    for i in 0..n {
        if full(out, max) {
            return;
        }
        let idx_seg = format!("decls[{}]", i);
        match (as_.get(i), bs.get(i)) {
            (Some(da), Some(db)) => {
                let kind_a = decl_kind(da);
                let kind_b = decl_kind(db);
                if kind_a != kind_b || da.name() != db.name() {
                    let seg = format!("{} ({} {})", idx_seg, kind_a, da.name());
                    push_div(
                        path,
                        Some(&seg),
                        Some(da.span()),
                        Some(db.span()),
                        json_snippet(da),
                        json_snippet(db),
                        out,
                    );
                } else {
                    let seg = format!("{} ({} {})", idx_seg, kind_a, da.name());
                    path.push(seg);
                    compare_decl(da, db, path, out, max);
                    path.pop();
                }
            }
            (Some(da), None) => {
                push_div(
                    path,
                    Some(&idx_seg),
                    Some(da.span()),
                    None,
                    json_snippet(da),
                    "<missing>".to_string(),
                    out,
                );
            }
            (None, Some(db)) => {
                push_div(
                    path,
                    Some(&idx_seg),
                    None,
                    Some(db.span()),
                    "<missing>".to_string(),
                    json_snippet(db),
                    out,
                );
            }
            (None, None) => unreachable!(),
        }
    }
}

fn compare_decl(
    da: &Decl,
    db: &Decl,
    path: &mut Vec<String>,
    out: &mut Vec<Divergence>,
    max: usize,
) {
    if full(out, max) {
        return;
    }
    match (da, db) {
        (Decl::Import { from: fa, .. }, Decl::Import { from: fb, .. }) => {
            if fa != fb {
                push_div(
                    path,
                    Some("from"),
                    Some(da.span()),
                    Some(db.span()),
                    format!("{:?}", fa),
                    format!("{:?}", fb),
                    out,
                );
            }
        }
        (
            Decl::Backend {
                none: na, body: ba, ..
            },
            Decl::Backend {
                none: nb, body: bb, ..
            },
        ) => {
            if na != nb {
                push_div(
                    path,
                    Some("none"),
                    Some(da.span()),
                    Some(db.span()),
                    na.to_string(),
                    nb.to_string(),
                    out,
                );
                return;
            }
            match (ba, bb) {
                (Some(fa), Some(fb)) => compare_fields(fa, fb, path, out, max),
                (None, None) => {}
                (Some(fa), None) => push_div(
                    path,
                    Some("body"),
                    Some(da.span()),
                    Some(db.span()),
                    json_snippet(fa),
                    "<none>".to_string(),
                    out,
                ),
                (None, Some(fb)) => push_div(
                    path,
                    Some("body"),
                    Some(da.span()),
                    Some(db.span()),
                    "<none>".to_string(),
                    json_snippet(fb),
                    out,
                ),
            }
        }
        (Decl::Probe { body: ba, .. }, Decl::Probe { body: bb, .. }) => {
            compare_fields(ba, bb, path, out, max)
        }
        (Decl::Acl { entries: ea, .. }, Decl::Acl { entries: eb, .. }) => {
            compare_acl_entries(ea, eb, path, out, max)
        }
        (Decl::Sub { body: ba, .. }, Decl::Sub { body: bb, .. }) => {
            compare_stmt_list(ba, bb, path, out, max, "body")
        }
        _ => unreachable!("kind already checked equal by caller"),
    }
}

fn compare_fields(
    fa: &[Field],
    fb: &[Field],
    path: &mut Vec<String>,
    out: &mut Vec<Divergence>,
    max: usize,
) {
    let n = fa.len().max(fb.len());
    for i in 0..n {
        if full(out, max) {
            return;
        }
        let idx_seg = format!("fields[{}]", i);
        match (fa.get(i), fb.get(i)) {
            (Some(a), Some(b)) => {
                if a.name != b.name {
                    let seg = format!("{} (.{})", idx_seg, a.name);
                    push_div(
                        path,
                        Some(&seg),
                        Some(a.span),
                        Some(b.span),
                        json_snippet(a),
                        json_snippet(b),
                        out,
                    );
                    continue;
                }
                let seg = format!(".{}", a.name);
                path.push(seg);
                compare_field_value(&a.value, &b.value, a.span, b.span, path, out, max);
                path.pop();
            }
            (Some(a), None) => push_div(
                path,
                Some(&idx_seg),
                Some(a.span),
                None,
                json_snippet(a),
                "<missing>".to_string(),
                out,
            ),
            (None, Some(b)) => push_div(
                path,
                Some(&idx_seg),
                None,
                Some(b.span),
                "<missing>".to_string(),
                json_snippet(b),
                out,
            ),
            (None, None) => unreachable!(),
        }
    }
}

fn compare_field_value(
    va: &FieldValue,
    vb: &FieldValue,
    span_a: Span,
    span_b: Span,
    path: &mut Vec<String>,
    out: &mut Vec<Divergence>,
    max: usize,
) {
    if full(out, max) {
        return;
    }
    let ka = field_value_kind(va);
    let kb = field_value_kind(vb);
    if ka != kb {
        push_div(
            path,
            None,
            Some(span_a),
            Some(span_b),
            json_snippet(va),
            json_snippet(vb),
            out,
        );
        return;
    }
    match (va, vb) {
        (FieldValue::Expr(ea), FieldValue::Expr(eb)) => {
            compare_expr(ea, eb, span_a, span_b, path, out, max)
        }
        (FieldValue::Probe(pa), FieldValue::Probe(pb)) => compare_fields(pa, pb, path, out, max),
        (FieldValue::ProbeRef(ra), FieldValue::ProbeRef(rb)) => {
            if ra != rb {
                push_div(
                    path,
                    None,
                    Some(span_a),
                    Some(span_b),
                    ra.clone(),
                    rb.clone(),
                    out,
                );
            }
        }
        (FieldValue::StringList(la), FieldValue::StringList(lb)) => {
            if la != lb {
                push_div(
                    path,
                    None,
                    Some(span_a),
                    Some(span_b),
                    format!("{:?}", la),
                    format!("{:?}", lb),
                    out,
                );
            }
        }
        _ => unreachable!("kind already checked equal above"),
    }
}

fn compare_acl_entries(
    ea: &[AclEntry],
    eb: &[AclEntry],
    path: &[String],
    out: &mut Vec<Divergence>,
    max: usize,
) {
    let n = ea.len().max(eb.len());
    for i in 0..n {
        if full(out, max) {
            return;
        }
        let idx_seg = format!("entries[{}]", i);
        match (ea.get(i), eb.get(i)) {
            (Some(a), Some(b)) => {
                if a.negated != b.negated || a.addr != b.addr || a.mask != b.mask {
                    push_div(
                        path,
                        Some(&idx_seg),
                        Some(a.span),
                        Some(b.span),
                        json_snippet(a),
                        json_snippet(b),
                        out,
                    );
                }
            }
            (Some(a), None) => push_div(
                path,
                Some(&idx_seg),
                Some(a.span),
                None,
                json_snippet(a),
                "<missing>".to_string(),
                out,
            ),
            (None, Some(b)) => push_div(
                path,
                Some(&idx_seg),
                None,
                Some(b.span),
                "<missing>".to_string(),
                json_snippet(b),
                out,
            ),
            (None, None) => unreachable!(),
        }
    }
}

// ─────────────────────────── stmts ───────────────────────────

fn compare_stmt_list(
    sa: &[Stmt],
    sb: &[Stmt],
    path: &mut Vec<String>,
    out: &mut Vec<Divergence>,
    max: usize,
    prefix: &str,
) {
    let n = sa.len().max(sb.len());
    for i in 0..n {
        if full(out, max) {
            return;
        }
        let idx_seg = format!("{}[{}]", prefix, i);
        match (sa.get(i), sb.get(i)) {
            (Some(a), Some(b)) => {
                let ka = stmt_kind(a);
                let kb = stmt_kind(b);
                if ka != kb {
                    push_div(
                        path,
                        Some(&idx_seg),
                        Some(a.span()),
                        Some(b.span()),
                        printer::stmt_to_string(a),
                        printer::stmt_to_string(b),
                        out,
                    );
                    continue;
                }
                path.push(idx_seg);
                path.push(ka.to_string());
                compare_stmt(a, b, path, out, max);
                path.pop();
                path.pop();
            }
            (Some(a), None) => push_div(
                path,
                Some(&idx_seg),
                Some(a.span()),
                None,
                printer::stmt_to_string(a),
                "<missing>".to_string(),
                out,
            ),
            (None, Some(b)) => push_div(
                path,
                Some(&idx_seg),
                None,
                Some(b.span()),
                "<missing>".to_string(),
                printer::stmt_to_string(b),
                out,
            ),
            (None, None) => unreachable!(),
        }
    }
}

fn compare_stmt(
    sa: &Stmt,
    sb: &Stmt,
    path: &mut Vec<String>,
    out: &mut Vec<Divergence>,
    max: usize,
) {
    if full(out, max) {
        return;
    }
    let span_a = sa.span();
    let span_b = sb.span();
    match (sa, sb) {
        (
            Stmt::Set {
                lhs: la, rhs: ra, ..
            },
            Stmt::Set {
                lhs: lb, rhs: rb, ..
            },
        ) => {
            if la != lb {
                push_div(
                    path,
                    Some("lhs"),
                    Some(span_a),
                    Some(span_b),
                    printer::stmt_to_string(sa),
                    printer::stmt_to_string(sb),
                    out,
                );
                return;
            }
            path.push("rhs".to_string());
            compare_expr(ra, rb, span_a, span_b, path, out, max);
            path.pop();
        }
        (Stmt::Unset { lhs: la, .. }, Stmt::Unset { lhs: lb, .. }) => {
            if la != lb {
                push_div(
                    path,
                    None,
                    Some(span_a),
                    Some(span_b),
                    printer::stmt_to_string(sa),
                    printer::stmt_to_string(sb),
                    out,
                );
            }
        }
        (Stmt::Call { sub: ca, .. }, Stmt::Call { sub: cb, .. }) => {
            if ca != cb {
                push_div(
                    path,
                    None,
                    Some(span_a),
                    Some(span_b),
                    printer::stmt_to_string(sa),
                    printer::stmt_to_string(sb),
                    out,
                );
            }
        }
        (Stmt::Return { action: aa, .. }, Stmt::Return { action: ab, .. }) => match (aa, ab) {
            (None, None) => {}
            (Some(a), Some(b)) => {
                if a.name != b.name {
                    push_div(
                        path,
                        Some("action"),
                        Some(span_a),
                        Some(span_b),
                        printer::stmt_to_string(sa),
                        printer::stmt_to_string(sb),
                        out,
                    );
                    return;
                }
                compare_expr_list(
                    &a.args,
                    &b.args,
                    span_a,
                    span_b,
                    path,
                    out,
                    max,
                    "action.args",
                );
            }
            _ => push_div(
                path,
                None,
                Some(span_a),
                Some(span_b),
                printer::stmt_to_string(sa),
                printer::stmt_to_string(sb),
                out,
            ),
        },
        (Stmt::Synthetic { value: va, .. }, Stmt::Synthetic { value: vb, .. }) => {
            path.push("value".to_string());
            compare_expr(va, vb, span_a, span_b, path, out, max);
            path.pop();
        }
        (
            Stmt::If {
                arms: arms_a,
                else_body: else_a,
                ..
            },
            Stmt::If {
                arms: arms_b,
                else_body: else_b,
                ..
            },
        ) => {
            if arms_a.len() != arms_b.len() {
                push_div(
                    path,
                    None,
                    Some(span_a),
                    Some(span_b),
                    printer::stmt_to_string(sa),
                    printer::stmt_to_string(sb),
                    out,
                );
                return;
            }
            for j in 0..arms_a.len() {
                if full(out, max) {
                    return;
                }
                let (cond_a, body_a) = &arms_a[j];
                let (cond_b, body_b) = &arms_b[j];
                let cond_seg = format!("arms[{}].cond", j);
                path.push(cond_seg);
                compare_expr(cond_a, cond_b, span_a, span_b, path, out, max);
                path.pop();
                if full(out, max) {
                    return;
                }
                compare_stmt_list(body_a, body_b, path, out, max, &format!("arms[{}].body", j));
            }
            if full(out, max) {
                return;
            }
            match (else_a, else_b) {
                (None, None) => {}
                (Some(a), Some(b)) => compare_stmt_list(a, b, path, out, max, "else_body"),
                _ => push_div(
                    path,
                    Some("else_body"),
                    Some(span_a),
                    Some(span_b),
                    printer::stmt_to_string(sa),
                    printer::stmt_to_string(sb),
                    out,
                ),
            }
        }
        (
            Stmt::New {
                name: na,
                vmod: vma,
                ctor: ca,
                args: aa,
                ..
            },
            Stmt::New {
                name: nb,
                vmod: vmb,
                ctor: cb,
                args: ab,
                ..
            },
        ) => {
            if na != nb || vma != vmb || ca != cb {
                push_div(
                    path,
                    None,
                    Some(span_a),
                    Some(span_b),
                    printer::stmt_to_string(sa),
                    printer::stmt_to_string(sb),
                    out,
                );
                return;
            }
            compare_arg_list(aa, ab, span_a, span_b, path, out, max, "args");
        }
        (Stmt::Expr { expr: ea, .. }, Stmt::Expr { expr: eb, .. }) => {
            compare_expr(ea, eb, span_a, span_b, path, out, max)
        }
        _ => unreachable!("kind already checked equal by caller"),
    }
}

// ─────────────────────────── exprs ───────────────────────────

/// Structural equality for `Expr` (spans don't exist on `Expr`, so this is a
/// plain deep-equality check via the span-free `Serialize` impl).
fn expr_eq(a: &Expr, b: &Expr) -> bool {
    serde_json::to_value(a).unwrap_or(serde_json::Value::Null)
        == serde_json::to_value(b).unwrap_or(serde_json::Value::Null)
}

/// Compares two expressions as a single unit: expressions are not broken
/// down into a finer-grained human path (no `.lhs`/`.rhs`/`.args[i]`
/// segments) — the path built by the caller (e.g. `arms[0].cond`, `rhs`)
/// already identifies the expression; a mismatch anywhere inside it is
/// reported once, with the whole expression pretty-printed on each side.
fn compare_expr(
    ea: &Expr,
    eb: &Expr,
    span_a: Span,
    span_b: Span,
    path: &[String],
    out: &mut Vec<Divergence>,
    max: usize,
) {
    if full(out, max) {
        return;
    }
    if !expr_eq(ea, eb) {
        push_div(
            path,
            None,
            Some(span_a),
            Some(span_b),
            printer::expr_to_string(ea),
            printer::expr_to_string(eb),
            out,
        );
    }
}

// `path`/`out`/`max`/`span_a`/`span_b` are a well-understood, already-tested
// threading cluster shared by every compare_* helper in this file; bundling
// them into a context struct is a bigger refactor than this lint warrants.
#[allow(clippy::too_many_arguments)]
fn compare_expr_list(
    la: &[Expr],
    lb: &[Expr],
    span_a: Span,
    span_b: Span,
    path: &mut Vec<String>,
    out: &mut Vec<Divergence>,
    max: usize,
    prefix: &str,
) {
    let n = la.len().max(lb.len());
    for i in 0..n {
        if full(out, max) {
            return;
        }
        let idx_seg = format!("{}[{}]", prefix, i);
        match (la.get(i), lb.get(i)) {
            (Some(a), Some(b)) => {
                path.push(idx_seg);
                compare_expr(a, b, span_a, span_b, path, out, max);
                path.pop();
            }
            (Some(a), None) => push_div(
                path,
                Some(&idx_seg),
                Some(span_a),
                Some(span_b),
                printer::expr_to_string(a),
                "<missing>".to_string(),
                out,
            ),
            (None, Some(b)) => push_div(
                path,
                Some(&idx_seg),
                Some(span_a),
                Some(span_b),
                "<missing>".to_string(),
                printer::expr_to_string(b),
                out,
            ),
            (None, None) => unreachable!(),
        }
    }
}

#[allow(clippy::too_many_arguments)] // see compare_expr_list
fn compare_arg_list(
    aa: &[Arg],
    ab: &[Arg],
    span_a: Span,
    span_b: Span,
    path: &mut Vec<String>,
    out: &mut Vec<Divergence>,
    max: usize,
    prefix: &str,
) {
    let n = aa.len().max(ab.len());
    for i in 0..n {
        if full(out, max) {
            return;
        }
        let idx_seg = format!("{}[{}]", prefix, i);
        match (aa.get(i), ab.get(i)) {
            (Some(a), Some(b)) => {
                if a.name != b.name {
                    push_div(
                        path,
                        Some(&idx_seg),
                        Some(span_a),
                        Some(span_b),
                        json_snippet(a),
                        json_snippet(b),
                        out,
                    );
                    continue;
                }
                path.push(idx_seg);
                compare_expr(&a.value, &b.value, span_a, span_b, path, out, max);
                path.pop();
            }
            (Some(a), None) => push_div(
                path,
                Some(&idx_seg),
                Some(span_a),
                Some(span_b),
                json_snippet(a),
                "<missing>".to_string(),
                out,
            ),
            (None, Some(b)) => push_div(
                path,
                Some(&idx_seg),
                Some(span_a),
                Some(span_b),
                "<missing>".to_string(),
                json_snippet(b),
                out,
            ),
            (None, None) => unreachable!(),
        }
    }
}

// ─────────────────────────── report rendering ───────────────────────────

/// Renders a divergence report in the format described by the spec:
/// one block per divergence (human path, then `A:`/`B:` location+snippet
/// lines), blank line between blocks. If `total_capped`, a trailing note
/// is appended stating more divergences exist beyond the ones reported.
pub fn render_report(
    divs: &[Divergence],
    sm_a: &ast::SourceMap,
    sm_b: &ast::SourceMap,
    total_capped: bool,
) -> String {
    let mut out = String::new();
    for (i, d) in divs.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&d.path);
        out.push('\n');

        let loc_a = render_loc(sm_a, d.span_a);
        let loc_b = render_loc(sm_b, d.span_b);

        out.push_str(&format!("  A: {}      {}\n", loc_a, d.snippet_a));
        out.push_str(&format!("  B: {}      {}\n", loc_b, d.snippet_b));
    }
    if total_capped {
        out.push_str("\n… and more divergences exist beyond the reported cap\n");
    }
    out
}

fn render_loc(sm: &ast::SourceMap, span: Option<Span>) -> String {
    match span {
        Some(sp) if (sp.file as usize) < sm.files.len() => {
            let (path, line, col) = sm.resolve(sp);
            format!("{}:{}:{}", path.display(), line, col)
        }
        _ => "?:?:?".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::builder::*;
    use pretty_assertions::assert_eq;
    use std::path::PathBuf;

    #[test]
    fn d1_equal_asts_produce_no_divergences() {
        let a = program(vec![
            backend("b1", vec![fexpr("host", str_("example.com"))]),
            sub(
                "vcl_recv",
                vec![
                    set(&["req", "http", "x"], str_("1")),
                    ret_action("lookup", vec![]),
                ],
            ),
        ]);
        let b = program(vec![
            backend("b1", vec![fexpr("host", str_("example.com"))]),
            sub(
                "vcl_recv",
                vec![
                    set(&["req", "http", "x"], str_("1")),
                    ret_action("lookup", vec![]),
                ],
            ),
        ]);

        let divs = compare(&a, &b, 20);
        assert_eq!(divs, Vec::new());
    }

    #[test]
    fn d2_single_mismatch_reports_one_entry_with_correct_path_no_descent() {
        let a = program(vec![sub(
            "vcl_recv",
            vec![
                call("vcl_hit"),
                call("vcl_hit"),
                if_(
                    vec![(
                        bin(
                            ast::BinOp::Match,
                            var(&["req", "http", "cookie"]),
                            str_("session"),
                        ),
                        vec![ret_action("hash", vec![])],
                    )],
                    None,
                ),
            ],
        )]);
        let b = program(vec![sub(
            "vcl_recv",
            vec![
                call("vcl_hit"),
                call("vcl_hit"),
                if_(
                    vec![(
                        bin(
                            ast::BinOp::Match,
                            var(&["req", "http", "cookie"]),
                            str_("sessionid"),
                        ),
                        vec![ret_action("hash", vec![])],
                    )],
                    None,
                ),
            ],
        )]);

        let divs = compare(&a, &b, 20);
        assert_eq!(divs.len(), 1, "expected exactly one divergence: {:?}", divs);
        let d = &divs[0];
        assert_eq!(
            d.path,
            "decls[0] (sub vcl_recv) › body[2] › if › arms[0].cond"
        );
        assert_eq!(d.snippet_a, "req.http.cookie ~ \"session\"");
        assert_eq!(d.snippet_b, "req.http.cookie ~ \"sessionid\"");
    }

    #[test]
    fn d3_max_reports_caps_number_of_entries() {
        // 5 subs, each with one mismatching `set` statement -> 5 divergences available.
        let mut a_decls = Vec::new();
        let mut b_decls = Vec::new();
        for i in 0..5 {
            let name = format!("vcl_recv_{}", i);
            // custom subs (not builtin) so sort pass ordering is by name; identical
            // names/kinds keep decls aligned, only the body differs.
            a_decls.push(sub(&name, vec![set(&["req", "http", "x"], str_("a"))]));
            b_decls.push(sub(&name, vec![set(&["req", "http", "x"], str_("b"))]));
        }
        let a = program(a_decls);
        let b = program(b_decls);

        let divs = compare(&a, &b, 2);
        assert_eq!(divs.len(), 2, "should stop at max_reports: {:?}", divs);

        let report = render_report(
            divs.as_slice(),
            &ast::SourceMap::default(),
            &ast::SourceMap::default(),
            true,
        );
        assert!(
            report.contains("more divergences"),
            "capped report should note more divergences exist: {}",
            report
        );
    }

    #[test]
    fn d4_dual_spans_resolve_to_correct_included_file() {
        // Build source maps with 2 files each, simulating an include: file 0 is the
        // "main" file, file 1 is the "included" file. The differing node's span
        // points at file 1 in both source maps.
        let mut sm_a = ast::SourceMap::default();
        sm_a.add(PathBuf::from("main_a.vcl"), "vcl 4.1;\n".to_string());
        sm_a.add(
            PathBuf::from("included_a.vcl"),
            "sub vcl_recv {\n    set req.http.x = \"a\";\n}\n".to_string(),
        );

        let mut sm_b = ast::SourceMap::default();
        sm_b.add(PathBuf::from("main_b.vcl"), "vcl 4.1;\n".to_string());
        sm_b.add(
            PathBuf::from("included_b.vcl"),
            "sub vcl_recv {\n    set req.http.x = \"b\";\n}\n".to_string(),
        );

        // Span pointing at `"a"`/`"b"` on line 2 of the included file (file index 1).
        let span_a = ast::Span {
            file: 1,
            lo: 34,
            hi: 37,
        };
        let span_b = ast::Span {
            file: 1,
            lo: 34,
            hi: 37,
        };

        let divs = vec![Divergence {
            path: "decls[0] (sub vcl_recv) › body[0] › set › rhs".to_string(),
            span_a: Some(span_a),
            span_b: Some(span_b),
            snippet_a: "\"a\"".to_string(),
            snippet_b: "\"b\"".to_string(),
        }];

        let report = render_report(&divs, &sm_a, &sm_b, false);
        assert!(
            report.contains("included_a.vcl:2:"),
            "should resolve to the included file for A: {}",
            report
        );
        assert!(
            report.contains("included_b.vcl:2:"),
            "should resolve to the included file for B: {}",
            report
        );
        assert!(
            !report.contains("main_a.vcl") && !report.contains("main_b.vcl"),
            "should not resolve to the main file: {}",
            report
        );
    }

    #[test]
    fn compare_handles_none_spans_with_placeholder() {
        // Dummy spans from ast::builder resolve to file 0 which may not exist in an
        // empty SourceMap; render_loc must fall back to a placeholder rather than
        // panicking on out-of-bounds access.
        let a = program(vec![sub("vcl_recv", vec![call("vcl_hit")])]);
        let b = program(vec![sub("vcl_recv", vec![call("vcl_miss")])]);
        let divs = compare(&a, &b, 20);
        assert_eq!(divs.len(), 1);

        let empty_sm = ast::SourceMap::default();
        let report = render_report(&divs, &empty_sm, &empty_sm, false);
        assert!(
            report.contains("?:?:?"),
            "out-of-range span should render as placeholder: {}",
            report
        );
    }

    #[test]
    fn d5_merged_multi_fragment_sub_divergence_reports_correct_body_index() {
        // compare() runs on already-normalized programs, so a sub that VCL
        // source split into several `sub name { ... }` fragments has already
        // been merged (by normalize::rename::merge_subs) into one Decl::Sub
        // with a concatenated body by the time it gets here. Build that
        // merged shape directly: body [X, Z] in A vs [X, W] in B — same
        // first statement (as if the first fragment matched), diverging in
        // the second (as if the second fragment differed).
        let a = program(vec![sub(
            "vcl_recv",
            vec![
                set(&["req", "http", "x"], str_("X")),
                set(&["req", "http", "z"], str_("Z")),
            ],
        )]);
        let b = program(vec![sub(
            "vcl_recv",
            vec![
                set(&["req", "http", "x"], str_("X")),
                set(&["req", "http", "z"], str_("W")),
            ],
        )]);

        let divs = compare(&a, &b, 20);
        assert_eq!(divs.len(), 1, "expected exactly one divergence: {:?}", divs);
        let d = &divs[0];
        // The path indexes into the merged body ("body[1]"), not a fragment
        // number — there is no fragment concept left post-merge.
        assert_eq!(d.path, "decls[0] (sub vcl_recv) › body[1] › set › rhs");
        assert_eq!(d.snippet_a, "\"Z\"");
        assert_eq!(d.snippet_b, "\"W\"");
    }
}
