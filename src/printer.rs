//! Canonical VCL pretty-printer.
//!
//! Outputs deterministic, re-parseable VCL from a normalized AST.
//! Rules: 4-space indent, K&R braces, one statement per line,
//! strings as "..." or {...} for special chars, durations as <n>s, bytes as <n>B.
//!
//! Comments (see `ast::CommentMap`) are attached by `Span`, not carried on
//! the node itself, so every function below that renders a Decl/Stmt/
//! Field/AclEntry/Arg takes the whole map and looks its node up by span.
//! They're print-only trivia -- `canon.rs`/`compare.rs` never see them.

use crate::ast;

/// Prints a normalized Program as canonical VCL text.
pub fn print(p: &ast::Program) -> String {
    let mut lines = vec!["vcl 4.1;".to_string()];

    for (i, decl) in p.decls.iter().enumerate() {
        lines.extend(print_decl(decl, &p.comments, i == 0));
    }

    for (i, c) in p.trailing_comments.iter().enumerate() {
        if i == 0 {
            lines.push(String::new());
        }
        render_comment_block(&mut lines, 0, c);
    }

    lines.join("\n")
}

/// The blank-line-before-leading-comments convention for a given list kind.
enum BlankRule {
    /// Top-level decls: always got a blank line before them pre-comments
    /// (comment or not) -- unless this is the first item in the list.
    Always,
    /// Statements, fields, acl entries: zero blank lines by default, one
    /// gained only when an actual leading-comment block is present (and
    /// this isn't the first item).
    IfLeading,
    /// Args: never -- a one-arg-per-line call is already a rare fallback
    /// rendering, and a blank line between a comma and the next arg's
    /// comment reads as visual noise no normal call-argument style uses.
    Never,
}

/// Wraps a node's own rendered lines (`core`) with its leading/trailing/
/// after comments, keyed by `span`. `blank_rule` reproduces the
/// pre-comments spacing convention for the node's *list kind* (see
/// `BlankRule`).
fn wrap_comments(
    cm: &ast::CommentMap,
    span: ast::Span,
    indent_level: usize,
    is_first: bool,
    blank_rule: BlankRule,
    core: impl FnOnce() -> Vec<String>,
) -> Vec<String> {
    let nc = cm.get(span);
    let leading: &[ast::LeadingComment] = nc.map_or(&[], |c| &c.leading);
    let want_blank = match blank_rule {
        BlankRule::Always => true,
        BlankRule::IfLeading => !leading.is_empty(),
        BlankRule::Never => false,
    };
    let mut out = Vec::new();
    if !is_first && want_blank {
        out.push(String::new());
    }
    for c in leading {
        render_comment_block(&mut out, indent_level, c);
    }

    let mut core_lines = core();
    if let Some(trailing) = nc.and_then(|c| c.trailing.as_ref()) {
        // A same-line trailing comment always shares a line with the
        // node's own *header* token (e.g. `backend web {  // note` for a
        // multi-line decl) -- so it belongs on the first rendered line,
        // not the last. For single-line nodes (fields, acl entries, most
        // statements) first and last are the same line anyway.
        if let Some(first) = core_lines.first_mut() {
            first.push_str("  ");
            first.push_str(&trailing.replace('\n', " "));
        }
    }
    out.extend(core_lines);

    if let Some(after) = nc.map(|c| c.after.as_slice()).filter(|a| !a.is_empty()) {
        out.push(String::new());
        for c in after {
            render_comment_block(&mut out, indent_level, c);
        }
    }
    out
}

/// Renders one leading/after comment. A single-line comment (`#`/`//`, or a
/// `/* ... */` that never actually contains a newline) is just indented in
/// place. A genuine multi-line `/* ... */` block is dedented (using its
/// continuation lines' shared leading whitespace as the baseline) and then
/// re-indented as a whole, so internal relative alignment (ASCII art,
/// manually lined-up columns) survives the shift to a new indent level.
/// `unindented` (merge-reattached comments only, see
/// `normalize::rename::merge_only`) forces column 0 regardless of level.
fn render_comment_block(out: &mut Vec<String>, indent_level: usize, c: &ast::LeadingComment) {
    let indent = if c.unindented {
        String::new()
    } else {
        "    ".repeat(indent_level)
    };
    if !c.text.contains('\n') {
        out.push(format!("{indent}{}", c.text));
        return;
    }
    let mut lines = c.text.split('\n');
    let first = lines.next().unwrap_or("");
    let rest: Vec<&str> = lines.collect();
    let min_indent = rest
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start_matches([' ', '\t']).len())
        .min()
        .unwrap_or(0);
    out.push(format!("{indent}{first}"));
    for l in rest {
        let stripped = l.get(min_indent..).unwrap_or_else(|| l.trim_start());
        out.push(format!("{indent}{stripped}"));
    }
}

fn print_decl(d: &ast::Decl, cm: &ast::CommentMap, is_first: bool) -> Vec<String> {
    wrap_comments(cm, d.span(), 0, is_first, BlankRule::Always, || match d {
        ast::Decl::Import { name, from, .. } => {
            vec![if let Some(f) = from {
                format!("import {} from {};", name, format_string(f))
            } else {
                format!("import {};", name)
            }]
        }

        ast::Decl::Backend {
            name, none, body, ..
        } => {
            if *none {
                vec![format!("backend {} none;", name)]
            } else if let Some(fields) = body {
                let mut lines = vec![format!("backend {} {{", name)];
                lines.extend(print_fields(fields, cm, 1));
                lines.push("}".to_string());
                lines
            } else {
                vec![format!("backend {} {{}};", name)]
            }
        }

        ast::Decl::Probe { name, body, .. } => {
            let mut lines = vec![format!("probe {} {{", name)];
            lines.extend(print_fields(body, cm, 1));
            lines.push("}".to_string());
            lines
        }

        ast::Decl::Acl { name, entries, .. } => {
            let mut lines = vec![format!("acl {} {{", name)];
            for (i, entry) in entries.iter().enumerate() {
                lines.extend(wrap_comments(
                    cm,
                    entry.span,
                    1,
                    i == 0,
                    BlankRule::IfLeading,
                    || vec![format!("    {}", acl_entry_to_string(entry))],
                ));
            }
            lines.push("}".to_string());
            lines
        }

        ast::Decl::Sub { name, body, .. } => {
            let mut lines = vec![format!("sub {} {{", name)];
            lines.extend(print_stmts(body, cm, 1));
            lines.push("}".to_string());
            lines
        }
    })
}

fn print_fields(fields: &[ast::Field], cm: &ast::CommentMap, indent_level: usize) -> Vec<String> {
    let indent = "    ".repeat(indent_level);
    let mut lines = Vec::new();
    for (i, field) in fields.iter().enumerate() {
        lines.extend(wrap_comments(
            cm,
            field.span,
            indent_level,
            i == 0,
            BlankRule::IfLeading,
            || vec![format!("{indent}{}", field_to_string(field, cm))],
        ));
    }
    lines
}

fn print_stmts(stmts: &[ast::Stmt], cm: &ast::CommentMap, indent_level: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for (i, stmt) in stmts.iter().enumerate() {
        lines.extend(wrap_comments(
            cm,
            stmt.span(),
            indent_level,
            i == 0,
            BlankRule::IfLeading,
            || stmt_to_lines(stmt, indent_level, cm),
        ));
    }
    lines
}

/// Prints a whole `.name = value;` field line, including the trailing
/// separator. An inline probe block (`.probe = { ... }`) is the one
/// exception: its closing '}' is the terminator and real VCC rejects a
/// trailing ';' after it (confirmed against `varnishd -C`), unlike every
/// other field-value form.
fn field_to_string(field: &ast::Field, cm: &ast::CommentMap) -> String {
    match &field.value {
        ast::FieldValue::Probe(_) => {
            format!(
                ".{} = {}",
                field.name,
                field_value_to_string(&field.value, cm)
            )
        }
        _ => format!(
            ".{} = {};",
            field.name,
            field_value_to_string(&field.value, cm)
        ),
    }
}

fn field_value_to_string(fv: &ast::FieldValue, cm: &ast::CommentMap) -> String {
    match fv {
        ast::FieldValue::Expr(e) => expr_to_string(e),
        ast::FieldValue::ProbeRef(name) => name.clone(),
        ast::FieldValue::StringList(strs) => strs
            .iter()
            .map(|s| format_string(s))
            .collect::<Vec<_>>()
            .join(" "),
        ast::FieldValue::Probe(fields) => {
            let inner = print_fields(fields, cm, 2);
            let mut s = "{\n".to_string();
            for line in inner {
                s.push_str(&line);
                s.push('\n');
            }
            s.push_str("    }");
            s
        }
    }
}

fn acl_entry_to_string(e: &ast::AclEntry) -> String {
    let neg = if e.negated { "!" } else { "" };
    match e.mask {
        Some(m) => format!("{}{} / {};", neg, format_string(&e.addr), m),
        None => format!("{}{};", neg, format_string(&e.addr)),
    }
}

fn stmt_to_lines(s: &ast::Stmt, indent_level: usize, cm: &ast::CommentMap) -> Vec<String> {
    let indent = "    ".repeat(indent_level);

    match s {
        ast::Stmt::Set { lhs, rhs, .. } => {
            vec![format!(
                "{}set {} = {};",
                indent,
                lvalue_to_string(lhs),
                expr_to_string(rhs)
            )]
        }

        ast::Stmt::Unset { lhs, .. } => {
            vec![format!("{}unset {};", indent, lvalue_to_string(lhs))]
        }

        ast::Stmt::Call { sub, .. } => {
            vec![format!("{}call {};", indent, sub)]
        }

        ast::Stmt::Return { action, .. } => {
            if let Some(a) = action {
                let arg_strs: Vec<_> = a.args.iter().map(expr_to_string).collect();
                if arg_strs.is_empty() {
                    vec![format!("{}return ({});", indent, a.name)]
                } else {
                    vec![format!(
                        "{}return ({} ({}));",
                        indent,
                        a.name,
                        arg_strs.join(", ")
                    )]
                }
            } else {
                vec![format!("{}return;", indent)]
            }
        }

        ast::Stmt::Synthetic { value, .. } => {
            vec![format!("{}synthetic ({});", indent, expr_to_string(value))]
        }

        ast::Stmt::If {
            arms, else_body, ..
        } => {
            let mut lines = Vec::new();

            for (i, (cond, body)) in arms.iter().enumerate() {
                let if_or_elsif = if i == 0 { "if" } else { "else if" };
                lines.push(format!(
                    "{}{} ({}) {{",
                    indent,
                    if_or_elsif,
                    expr_to_string(cond)
                ));

                lines.extend(print_stmts(body, cm, indent_level + 1));

                if i < arms.len() - 1 {
                    lines.push(format!("{}}} ", indent));
                } else {
                    if else_body.is_some() {
                        lines.push(format!("{}}} ", indent));
                    } else {
                        lines.push(format!("{}}}", indent));
                    }
                }
            }

            if let Some(else_stmts) = else_body {
                let last_idx = lines.len() - 1;
                let last_line = lines[last_idx].trim_end().to_string() + " else {";
                lines[last_idx] = last_line;
                lines.extend(print_stmts(else_stmts, cm, indent_level + 1));
                lines.push(format!("{}}}", indent));
            }

            lines
        }

        ast::Stmt::New {
            name,
            vmod,
            ctor,
            args,
            ..
        } => print_call_like(
            &format!("{indent}new {name} = {vmod}.{ctor}"),
            &indent,
            args,
            cm,
            indent_level,
            " = ",
        ),

        ast::Stmt::Expr {
            expr: ast::Expr::Call { target, args },
            ..
        } => print_call_like(
            &format!("{indent}{}", target.join(".")),
            &indent,
            args,
            cm,
            indent_level,
            "=",
        ),

        ast::Stmt::Expr { expr, .. } => {
            vec![format!("{}{};", indent, expr_to_string(expr))]
        }
    }
}

/// Renders `{prefix}(args);` -- the common shape of a bare call statement
/// (`std.log(...)`) and a `new x = vmod.ctor(...)` constructor call. Stays
/// on one line unless some arg carries a comment, in which case it expands
/// to one arg per line so there's somewhere for that comment to go.
fn print_call_like(
    prefix: &str,
    indent: &str,
    args: &[ast::Arg],
    cm: &ast::CommentMap,
    indent_level: usize,
    named_sep: &str,
) -> Vec<String> {
    let visible: Vec<&ast::Arg> = args
        .iter()
        .filter(|a| !matches!(a.value, ast::Expr::Omitted))
        .collect();

    if !visible.iter().any(|a| has_comments(cm, a.span)) {
        let arg_strs: Vec<String> = visible
            .iter()
            .map(|a| arg_to_string(a, named_sep))
            .collect();
        return vec![format!("{prefix}({});", arg_strs.join(", "))];
    }

    let arg_indent = "    ".repeat(indent_level + 1);
    let mut lines = vec![format!("{prefix}(")];
    for (i, a) in visible.iter().enumerate() {
        let comma = if i + 1 < visible.len() { "," } else { "" };
        lines.extend(wrap_comments(
            cm,
            a.span,
            indent_level + 1,
            i == 0,
            BlankRule::Never,
            || {
                vec![format!(
                    "{arg_indent}{}{comma}",
                    arg_to_string(a, named_sep)
                )]
            },
        ));
    }
    lines.push(format!("{indent});"));
    lines
}

fn has_comments(cm: &ast::CommentMap, span: ast::Span) -> bool {
    cm.get(span)
        .is_some_and(|c| !c.leading.is_empty() || c.trailing.is_some() || !c.after.is_empty())
}

fn arg_to_string(a: &ast::Arg, named_sep: &str) -> String {
    if let Some(name) = &a.name {
        format!("{name}{named_sep}{}", expr_to_string(&a.value))
    } else {
        expr_to_string(&a.value)
    }
}

/// Prints an expression as a single-line string (no trailing newline).
pub fn expr_to_string(e: &ast::Expr) -> String {
    match e {
        ast::Expr::Str(s) => format_string(s),

        ast::Expr::Num(n) => n.clone(),

        ast::Expr::Duration(d) => format_duration(*d),

        ast::Expr::Bytes(b) => format!("{}B", b),

        ast::Expr::Bool(b) => if *b { "true" } else { "false" }.to_string(),

        ast::Expr::Omitted => String::new(),

        ast::Expr::CSource(c) => format!("C{{ {} }}C", c),

        ast::Expr::Var(parts) => parts.join("."),

        ast::Expr::Call { target, args } => {
            let target_str = target.join(".");
            let arg_strs: Vec<_> = args
                .iter()
                .filter(|a| !matches!(a.value, ast::Expr::Omitted))
                .map(|a| {
                    if let Some(name) = &a.name {
                        format!("{}={}", name, expr_to_string(&a.value))
                    } else {
                        expr_to_string(&a.value)
                    }
                })
                .collect();

            format!("{}({})", target_str, arg_strs.join(", "))
        }

        ast::Expr::Unary { op, expr } => {
            let op_str = match op {
                ast::UnOp::Not => "!",
                ast::UnOp::Neg => "-",
            };
            format!("{}{}", op_str, expr_to_string(expr))
        }

        ast::Expr::Binary { op, lhs, rhs } => {
            let op_str = match op {
                ast::BinOp::Eq => "==",
                ast::BinOp::Ne => "!=",
                ast::BinOp::Match => "~",
                ast::BinOp::NotMatch => "!~",
                ast::BinOp::Lt => "<",
                ast::BinOp::Le => "<=",
                ast::BinOp::Gt => ">",
                ast::BinOp::Ge => ">=",
                ast::BinOp::And => "&&",
                ast::BinOp::Or => "||",
                ast::BinOp::Add => "+",
                ast::BinOp::Sub => "-",
                ast::BinOp::Mul => "*",
                ast::BinOp::Div => "/",
            };

            // Always parenthesize nested binaries for determinism and re-parseability
            let lhs_str = match &**lhs {
                ast::Expr::Binary { .. } => format!("({})", expr_to_string(lhs)),
                _ => expr_to_string(lhs),
            };

            let rhs_str = match &**rhs {
                ast::Expr::Binary { .. } => format!("({})", expr_to_string(rhs)),
                _ => expr_to_string(rhs),
            };

            format!("{} {} {}", lhs_str, op_str, rhs_str)
        }
    }
}

/// Prints a statement as a single-line string (no trailing newline).
pub fn stmt_to_string(s: &ast::Stmt) -> String {
    // Single-line version of statements (no indent, used by compare.rs for snippets)
    match s {
        ast::Stmt::Set { lhs, rhs, .. } => {
            format!("set {} = {}", lvalue_to_string(lhs), expr_to_string(rhs))
        }

        ast::Stmt::Unset { lhs, .. } => {
            format!("unset {}", lvalue_to_string(lhs))
        }

        ast::Stmt::Call { sub, .. } => {
            format!("call {}", sub)
        }

        ast::Stmt::Return { action, .. } => {
            if let Some(a) = action {
                let arg_strs: Vec<_> = a.args.iter().map(expr_to_string).collect();
                if arg_strs.is_empty() {
                    format!("return ({})", a.name)
                } else {
                    format!("return ({} ({}))", a.name, arg_strs.join(", "))
                }
            } else {
                "return".to_string()
            }
        }

        ast::Stmt::Synthetic { value, .. } => {
            format!("synthetic ({})", expr_to_string(value))
        }

        ast::Stmt::If {
            arms, else_body, ..
        } => {
            // Simplified single-line representation (not a perfect reproduction, but good enough for snippets)
            let cond_str = expr_to_string(&arms[0].0);
            if arms.len() == 1 && else_body.is_none() {
                format!("if ({}) {{ ... }}", cond_str)
            } else {
                format!("if ({}) {{ ... }} [else ...]", cond_str)
            }
        }

        ast::Stmt::New {
            name,
            vmod,
            ctor,
            args,
            ..
        } => {
            let arg_strs: Vec<_> = args
                .iter()
                .filter(|a| !matches!(a.value, ast::Expr::Omitted))
                .map(|a| {
                    if let Some(aname) = &a.name {
                        format!("{}={}", aname, expr_to_string(&a.value))
                    } else {
                        expr_to_string(&a.value)
                    }
                })
                .collect();

            format!("new {} = {}.{}({})", name, vmod, ctor, arg_strs.join(", "))
        }

        ast::Stmt::Expr { expr, .. } => expr_to_string(expr),
    }
}

fn lvalue_to_string(lv: &ast::Lvalue) -> String {
    lv.parts.join(".")
}

fn format_string(s: &str) -> String {
    if s.contains('"') || s.contains('\n') {
        format!("{{\"{}\"", s) + "}"
    } else {
        format!("\"{}\"", s)
    }
}

fn format_duration(d: f64) -> String {
    // Format as <n>s, trimming trailing zeros
    if d.fract() == 0.0 {
        // It's an integer
        format!("{}s", d as u64)
    } else {
        // Has decimal part; format and trim trailing zeros
        let s = format!("{}", d);
        let trimmed = s.trim_end_matches('0').trim_end_matches('.');
        format!("{}s", trimmed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::builder::*;

    #[test]
    fn c3_printer_deterministic_strings() {
        // String containing quote should use {"..."}
        let prog = program(vec![sub("vcl_recv", vec![expr_stmt(str_("test\"quote"))])]);
        let output = print(&prog);
        // Long string syntax is {" ... "} so {"test"quote"} is the correct output
        assert!(
            output.contains("{\"test\"quote\""),
            "String with quote should use {{\"...\"}} long-string syntax"
        );

        // Regular string should use "..."
        let prog2 = program(vec![sub("vcl_recv", vec![expr_stmt(str_("simple"))])]);
        let output2 = print(&prog2);
        assert!(
            output2.contains("\"simple\""),
            "Simple string should use \"...\" syntax"
        );
    }

    #[test]
    fn c3_printer_deterministic_durations() {
        // 60 seconds should be "60s" not "1m"
        let prog = program(vec![sub("vcl_recv", vec![expr_stmt(dur(60.0))])]);
        let output = print(&prog);
        assert!(output.contains("60s"), "Duration 60 should print as 60s");

        // 0.5 seconds should be "0.5s"
        let prog2 = program(vec![sub("vcl_recv", vec![expr_stmt(dur(0.5))])]);
        let output2 = print(&prog2);
        assert!(
            output2.contains("0.5s"),
            "Duration 0.5 should print as 0.5s"
        );

        // 3600 should be "3600s", not "1h"
        let prog3 = program(vec![sub("vcl_recv", vec![expr_stmt(dur(3600.0))])]);
        let output3 = print(&prog3);
        assert!(
            output3.contains("3600s"),
            "Duration 3600 should print as 3600s"
        );
    }

    #[test]
    fn c3_printer_deterministic_bytes() {
        let prog = program(vec![sub("vcl_recv", vec![expr_stmt(bytes(1024))])]);
        let output = print(&prog);
        assert!(output.contains("1024B"), "Bytes should print as <n>B");
    }

    #[test]
    fn c3_printer_nested_binary_parenthesized() {
        // a + b * c should be a + (b * c) with parens around nested binary
        let prog = program(vec![sub(
            "vcl_recv",
            vec![expr_stmt(bin(
                ast::BinOp::Add,
                num("1"),
                bin(ast::BinOp::Mul, num("2"), num("3")),
            ))],
        )]);
        let output = print(&prog);
        assert!(
            output.contains("1 + (2 * 3)"),
            "Nested binary should be parenthesized"
        );
    }

    #[test]
    fn c3_printer_omitted_args_skipped() {
        // Omitted arguments should not appear in output
        let args = vec![arg(num("1")), arg(ast::Expr::Omitted), arg(num("3"))];
        let prog = program(vec![sub(
            "vcl_recv",
            vec![expr_stmt(fcall(&["std", "log"], args))],
        )]);
        let output = print(&prog);
        // The output should have the call with arguments but omitted ones should be missing
        assert!(
            output.contains("std.log"),
            "Function call should be printed"
        );
    }

    #[test]
    fn printer_backend_with_fields() {
        let fields = vec![
            fexpr("host", str_("example.com")),
            fexpr("port", str_("8080")),
        ];
        let prog = program(vec![backend("web", fields)]);
        let output = print(&prog);

        assert!(
            output.contains("backend web {"),
            "Backend declaration should open"
        );
        assert!(
            output.contains(".host = \"example.com\";"),
            "First field should be present"
        );
        assert!(
            output.contains(".port = \"8080\";"),
            "Second field should be present"
        );
        assert!(output.contains("}"), "Backend should close");
    }

    #[test]
    fn printer_sub_with_statements() {
        let stmts = vec![
            set(&["req", "http", "x"], str_("1")),
            call("vcl_hit"),
            ret(None),
        ];
        let prog = program(vec![sub("vcl_recv", stmts)]);
        let output = print(&prog);

        assert!(
            output.contains("sub vcl_recv {"),
            "Sub declaration should open"
        );
        assert!(
            output.contains("set req.http.x = \"1\";"),
            "Set statement should be present"
        );
        assert!(
            output.contains("call vcl_hit;"),
            "Call statement should be present"
        );
        assert!(
            output.contains("return;"),
            "Return statement should be present"
        );
        assert!(output.contains("}"), "Sub should close");
    }

    #[test]
    fn printer_acl() {
        let entries = vec![
            acl_entry("1.2.3.4", Some(32), false),
            acl_entry("192.168.0.0", Some(16), false),
            acl_entry("10.0.0.0", Some(8), true),
        ];
        let prog = program(vec![acl("office", entries)]);
        let output = print(&prog);

        assert!(output.contains("acl office {"), "ACL should open");
        assert!(output.contains("\"1.2.3.4\" / 32;"), "ACL entry with mask");
        assert!(output.contains("!\"10.0.0.0\" / 8;"), "Negated ACL entry");
    }

    #[test]
    fn expr_to_string_binary_operator_spacing() {
        // Operators should have single spaces around them
        let expr = bin(ast::BinOp::Eq, var(&["a"]), var(&["b"]));
        let s = expr_to_string(&expr);
        assert_eq!(s, "a == b", "Binary operator should have single spaces");
    }

    #[test]
    fn expr_to_string_call_with_named_args() {
        let args = vec![arg(num("1")), narg("resolve", bool_(true))];
        let expr = fcall(&["std", "log"], args);
        let s = expr_to_string(&expr);
        assert!(s.contains("std.log"), "Call target should be correct");
        assert!(
            s.contains("resolve=true"),
            "Named argument should be formatted as name=value"
        );
    }

    #[test]
    fn printer_vcl_header() {
        let prog = program(vec![]);
        let output = print(&prog);
        assert!(
            output.starts_with("vcl 4.1;"),
            "Output should start with vcl 4.1; header"
        );
    }

    #[test]
    fn printer_blank_lines_between_decls() {
        let prog = program(vec![backend("b1", vec![]), backend("b2", vec![])]);
        let output = print(&prog);
        // Should have vcl 4.1; then blank line, then first backend, blank line, second backend
        let lines: Vec<&str> = output.lines().collect();
        // At least 4 lines: vcl 4.1; (1), blank (2), backend b1 (3), blank (4), backend b2 (5)
        assert!(
            lines.len() >= 5,
            "Should have blank lines between declarations"
        );
    }
}

#[cfg(test)]
mod comment_tests {
    use super::*;
    use crate::normalize::rename::merge_only;
    use crate::parser::parse_str;

    fn parse(body: &str) -> ast::Program {
        parse_str(&format!("vcl 4.1;\n{body}")).expect("parse ok")
    }

    #[test]
    fn pc1_first_decl_leading_comment_no_blank_line() {
        let p = parse("// leading\nimport std;\n");
        let out = print(&p);
        assert_eq!(out, "vcl 4.1;\n// leading\nimport std;");
    }

    #[test]
    fn pc2_later_decl_leading_comment_gets_blank_line() {
        let p = parse("import std;\n\n// leading\nimport other;\n");
        let out = print(&p);
        assert_eq!(out, "vcl 4.1;\nimport std;\n\n// leading\nimport other;");
    }

    #[test]
    fn pc3_trailing_comment_on_multiline_decl_header_line() {
        let p = parse("backend web { // note\n .host = \"1.2.3.4\";\n}\n");
        let out = print(&p);
        assert!(
            out.contains("backend web {  // note\n"),
            "trailing comment should attach to the opening line, got:\n{out}"
        );
    }

    #[test]
    fn pc4_inline_field_and_stmt_trailing_comments() {
        let p = parse("sub vcl_recv {\n    set req.http.x = \"1\"; // note\n}\n");
        let out = print(&p);
        assert!(out.contains("set req.http.x = \"1\";  // note"));
    }

    #[test]
    fn pc5_first_stmt_leading_comment_no_blank_line_after_brace() {
        let p = parse("sub vcl_recv {\n    // leading\n    return (lookup);\n}\n");
        let out = print(&p);
        assert!(
            out.contains("sub vcl_recv {\n    // leading\n    return (lookup);\n}"),
            "got:\n{out}"
        );
    }

    #[test]
    fn pc6_later_stmt_leading_comment_gets_blank_line() {
        let p = parse(
            "sub vcl_recv {\n    set req.http.x = \"1\";\n\n    // leading\n    return (lookup);\n}\n",
        );
        let out = print(&p);
        assert!(out.contains("set req.http.x = \"1\";\n\n    // leading\n    return (lookup);"));
    }

    #[test]
    fn pc7_orphan_comment_before_closing_brace_attaches_to_previous_stmt() {
        let p = parse("sub vcl_recv {\n    return (lookup);\n\n    // orphan\n}\n");
        let out = print(&p);
        assert!(
            out.contains("return (lookup);\n\n    // orphan\n}"),
            "got:\n{out}"
        );
    }

    #[test]
    fn pc8_eof_trailing_comment_survives_decl_reordering() {
        // sub, acl, backend, import is NOT kind-rank order -- sort.rs
        // reorders to import, backend, acl, sub. The trailing comment must
        // still land at the very end, not wherever `sub` (originally last)
        // ends up.
        let mut p = parse(
            "sub vcl_recv { return (lookup); }\nacl a { \"1.2.3.4\"/32; }\nbackend b { .host = \"x\"; }\nimport std;\n\n# eof\n",
        );
        crate::normalize::sort::run(&mut p);
        let out = print(&p);
        assert!(out.trim_end().ends_with("# eof"), "got:\n{out}");
        let sub_pos = out.find("sub vcl_recv").unwrap();
        let eof_pos = out.find("# eof").unwrap();
        assert!(
            eof_pos > sub_pos,
            "eof comment must print after sub, got:\n{out}"
        );
    }

    #[test]
    fn pc9_block_comment_preserves_relative_alignment_on_reindent() {
        let p = parse(
            "sub vcl_recv {\n    /* line one\n       extra indent\n    */\n    return (lookup);\n}\n",
        );
        let out = print(&p);
        let lines: Vec<&str> = out.lines().collect();
        let start = lines.iter().position(|l| l.contains("line one")).unwrap();
        assert_eq!(lines[start], "    /* line one");
        assert_eq!(lines[start + 1], "       extra indent");
        assert_eq!(lines[start + 2], "    */");
    }

    #[test]
    fn pc10_arg_comment_forces_one_per_line_no_blank_before_it() {
        let p = parse("sub vcl_recv {\n    std.log(\"a\", /* c */ \"b\");\n}\n");
        let out = print(&p);
        assert!(out.contains("std.log(\n"), "got:\n{out}");
        assert!(
            !out.contains("\n\n        /* c */"),
            "no blank line should precede an arg's leading comment, got:\n{out}"
        );
        assert!(out.contains("/* c */\n        \"b\""), "got:\n{out}");
    }

    #[test]
    fn pc11_merge_reattaches_fragment_decl_comment_unindented() {
        let mut p = parse(
            "sub vcl_recv {\n    set req.http.a = \"1\";\n}\n// second fragment\nsub vcl_recv {\n    set req.http.b = \"2\";\n}\n",
        );
        merge_only(&mut p);
        let out = print(&p);
        assert_eq!(p.decls.len(), 1, "fragments should be merged into one sub");
        assert!(
            out.contains("\n// second fragment\n    set req.http.b"),
            "reattached comment should be unindented (column 0), got:\n{out}"
        );
    }

    #[test]
    fn pc12_comments_never_affect_canonical_json() {
        let commented = parse("// note\nimport std;\n// another\n");
        let plain = parse("import std;\n");
        assert_eq!(
            crate::canon::to_string(&commented),
            crate::canon::to_string(&plain),
            "comments must never affect canonical JSON"
        );
    }
}
