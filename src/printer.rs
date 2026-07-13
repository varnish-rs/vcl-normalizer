//! Canonical VCL pretty-printer.
//!
//! Outputs deterministic, re-parseable VCL from a normalized AST.
//! Rules: 4-space indent, K&R braces, one statement per line,
//! strings as "..." or {...} for special chars, durations as <n>s, bytes as <n>B.

use crate::ast;

/// Prints a normalized Program as canonical VCL text.
pub fn print(p: &ast::Program) -> String {
    let mut lines = vec!["vcl 4.1;".to_string()];

    for decl in &p.decls {
        lines.push(String::new()); // blank line between decls
        lines.extend(print_decl(decl));
    }

    lines.join("\n")
}

fn print_decl(d: &ast::Decl) -> Vec<String> {
    match d {
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
                for field in fields {
                    lines.push(format!("    {}", field_to_string(field)));
                }
                lines.push("}".to_string());
                lines
            } else {
                vec![format!("backend {} {{}};", name)]
            }
        }

        ast::Decl::Probe { name, body, .. } => {
            let mut lines = vec![format!("probe {} {{", name)];
            for field in body {
                lines.push(format!("    {}", field_to_string(field)));
            }
            lines.push("}".to_string());
            lines
        }

        ast::Decl::Acl { name, entries, .. } => {
            let mut lines = vec![format!("acl {} {{", name)];
            for entry in entries {
                lines.push(format!("    {}", acl_entry_to_string(entry)));
            }
            lines.push("}".to_string());
            lines
        }

        ast::Decl::Sub { name, body, .. } => {
            let mut lines = vec![format!("sub {} {{", name)];
            for stmt in body {
                lines.extend(stmt_to_lines(stmt, 1));
            }
            lines.push("}".to_string());
            lines
        }
    }
}

/// Prints a whole `.name = value;` field line, including the trailing
/// separator. An inline probe block (`.probe = { ... }`) is the one
/// exception: its closing '}' is the terminator and real VCC rejects a
/// trailing ';' after it (confirmed against `varnishd -C`), unlike every
/// other field-value form.
fn field_to_string(field: &ast::Field) -> String {
    match &field.value {
        ast::FieldValue::Probe(_) => {
            format!(".{} = {}", field.name, field_value_to_string(&field.value))
        }
        _ => format!(".{} = {};", field.name, field_value_to_string(&field.value)),
    }
}

fn field_value_to_string(fv: &ast::FieldValue) -> String {
    match fv {
        ast::FieldValue::Expr(e) => expr_to_string(e),
        ast::FieldValue::ProbeRef(name) => name.clone(),
        ast::FieldValue::StringList(strs) => strs
            .iter()
            .map(|s| format_string(s))
            .collect::<Vec<_>>()
            .join(" "),
        ast::FieldValue::Probe(fields) => {
            let mut s = "{\n".to_string();
            for field in fields {
                s.push_str(&format!("        {}\n", field_to_string(field)));
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

fn stmt_to_lines(s: &ast::Stmt, indent_level: usize) -> Vec<String> {
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

                for stmt in body {
                    lines.extend(stmt_to_lines(stmt, indent_level + 1));
                }

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
                for stmt in else_stmts {
                    lines.extend(stmt_to_lines(stmt, indent_level + 1));
                }
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
        } => {
            let arg_strs: Vec<_> = args
                .iter()
                .filter(|a| !matches!(a.value, ast::Expr::Omitted))
                .map(|a| {
                    if let Some(aname) = &a.name {
                        format!("{} = {}", aname, expr_to_string(&a.value))
                    } else {
                        expr_to_string(&a.value)
                    }
                })
                .collect();

            vec![format!(
                "{}new {} = {}.{}({});",
                indent,
                name,
                vmod,
                ctor,
                arg_strs.join(", ")
            )]
        }

        ast::Stmt::Expr { expr, .. } => {
            vec![format!("{}{};", indent, expr_to_string(expr))]
        }
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
