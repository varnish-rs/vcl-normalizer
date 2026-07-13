//! Pass 2: vmod argument canonicalization
//!
//! For every `Expr::Call` (plain vmod function call or method call on a vmod
//! object instance) and `Stmt::New` (object constructor) whose vmod spec is
//! loaded:
//! 1. Map each argument to its parameter slot: positional args fill slots
//!    left-to-right; named args (`Arg.name`) fill their named slot. Error
//!    (exit-2 `VclError`) on duplicate slot or unknown named-arg name.
//! 2. Rewrite to fully-positional in declaration order.
//! 3. For every optional parameter left unfilled *or* filled with exactly its
//!    default literal (compared via `printer::expr_to_string`): represent the
//!    slot as `Expr::Omitted`. Trailing `Omitted` slots are then truncated.
//!
//! Calls to vmods/objects not found in `specs` are left completely untouched.

use crate::ast;
use crate::printer;
use crate::vmod::{Sig, VmodSpec};
use std::collections::BTreeMap;

/// Runs normalize pass 2 on `p` in place.
pub fn run(p: &mut ast::Program, specs: &BTreeMap<String, VmodSpec>) -> ast::Result<()> {
    let instances = collect_instances(p);
    for decl in &mut p.decls {
        if let ast::Decl::Sub { body, .. } = decl {
            process_stmts(body, specs, &instances)?;
        }
    }
    Ok(())
}

/// Scans every `Stmt::New` in the program (typically inside `vcl_init`, but we
/// don't assume that) to build a map from instance name to (vmod, ctor).
fn collect_instances(p: &ast::Program) -> BTreeMap<String, (String, String)> {
    let mut map = BTreeMap::new();
    for decl in &p.decls {
        if let ast::Decl::Sub { body, .. } = decl {
            collect_instances_stmts(body, &mut map);
        }
    }
    map
}

fn collect_instances_stmts(stmts: &[ast::Stmt], map: &mut BTreeMap<String, (String, String)>) {
    for s in stmts {
        match s {
            ast::Stmt::New {
                name, vmod, ctor, ..
            } => {
                map.insert(name.clone(), (vmod.clone(), ctor.clone()));
            }
            ast::Stmt::If {
                arms, else_body, ..
            } => {
                for (_, body) in arms {
                    collect_instances_stmts(body, map);
                }
                if let Some(eb) = else_body {
                    collect_instances_stmts(eb, map);
                }
            }
            _ => {}
        }
    }
}

fn process_stmts(
    stmts: &mut [ast::Stmt],
    specs: &BTreeMap<String, VmodSpec>,
    instances: &BTreeMap<String, (String, String)>,
) -> ast::Result<()> {
    for s in stmts {
        process_stmt(s, specs, instances)?;
    }
    Ok(())
}

fn process_stmt(
    s: &mut ast::Stmt,
    specs: &BTreeMap<String, VmodSpec>,
    instances: &BTreeMap<String, (String, String)>,
) -> ast::Result<()> {
    let span = s.span();
    match s {
        ast::Stmt::Set { rhs, .. } => process_expr(rhs, specs, instances, span)?,
        ast::Stmt::Unset { .. } => {}
        ast::Stmt::Call { .. } => {}
        ast::Stmt::Return { action, .. } => {
            if let Some(a) = action {
                for e in &mut a.args {
                    process_expr(e, specs, instances, span)?;
                }
            }
        }
        ast::Stmt::Synthetic { value, .. } => process_expr(value, specs, instances, span)?,
        ast::Stmt::If {
            arms, else_body, ..
        } => {
            for (cond, body) in arms {
                process_expr(cond, specs, instances, span)?;
                process_stmts(body, specs, instances)?;
            }
            if let Some(eb) = else_body {
                process_stmts(eb, specs, instances)?;
            }
        }
        ast::Stmt::New {
            vmod, ctor, args, ..
        } => {
            if let Some(spec) = specs.get(vmod) {
                if let Some(objspec) = spec.objects.get(ctor) {
                    rewrite_args(args, &objspec.init, span)?;
                }
            }
            for a in args.iter_mut() {
                process_expr(&mut a.value, specs, instances, span)?;
            }
        }
        ast::Stmt::Expr { expr, .. } => process_expr(expr, specs, instances, span)?,
    }
    Ok(())
}

fn process_expr(
    e: &mut ast::Expr,
    specs: &BTreeMap<String, VmodSpec>,
    instances: &BTreeMap<String, (String, String)>,
    span: ast::Span,
) -> ast::Result<()> {
    match e {
        ast::Expr::Call { target, args } => {
            if target.len() == 2 {
                let vmod_or_instance = target[0].clone();
                let func_or_method = target[1].clone();

                if let Some(spec) = specs.get(&vmod_or_instance) {
                    // Plain vmod function call: `vmod_name.func_name(...)`.
                    if let Some(sig) = spec.funcs.get(&func_or_method) {
                        rewrite_args(args, sig, span)?;
                    }
                } else if let Some((vmod_name, ctor_name)) = instances.get(&vmod_or_instance) {
                    // Method call on a known vmod object instance.
                    if let Some(spec) = specs.get(vmod_name) {
                        if let Some(objspec) = spec.objects.get(ctor_name) {
                            if let Some(sig) = objspec.methods.get(&func_or_method) {
                                rewrite_args(args, sig, span)?;
                            }
                        }
                    }
                }
            }

            for a in args.iter_mut() {
                process_expr(&mut a.value, specs, instances, span)?;
            }
        }
        ast::Expr::Unary { expr, .. } => process_expr(expr, specs, instances, span)?,
        ast::Expr::Binary { lhs, rhs, .. } => {
            process_expr(lhs, specs, instances, span)?;
            process_expr(rhs, specs, instances, span)?;
        }
        ast::Expr::Str(_)
        | ast::Expr::Num(_)
        | ast::Expr::Duration(_)
        | ast::Expr::Bytes(_)
        | ast::Expr::Bool(_)
        | ast::Expr::Omitted
        | ast::Expr::CSource(_)
        | ast::Expr::Var(_) => {}
    }
    Ok(())
}

/// Maps `args` onto `sig`'s parameter slots, rewrites them to fully
/// positional-in-declaration-order form, and collapses unfilled-or-default
/// optional trailing slots to `Expr::Omitted`.
fn rewrite_args(args: &mut Vec<ast::Arg>, sig: &Sig, span: ast::Span) -> ast::Result<()> {
    let n = sig.args.len();
    let mut slots: Vec<Option<ast::Expr>> = (0..n).map(|_| None).collect();
    let mut next_pos = 0usize;

    for a in args.drain(..) {
        let idx = match &a.name {
            None => {
                let i = next_pos;
                next_pos += 1;
                i
            }
            Some(name) => match sig
                .args
                .iter()
                .position(|s| s.name.as_deref() == Some(name.as_str()))
            {
                Some(i) => i,
                None => {
                    return Err(ast::VclError::new(
                        span,
                        format!("unknown vmod argument name '{}'", name),
                    ));
                }
            },
        };

        if idx >= n {
            return Err(ast::VclError::new(
                span,
                "too many positional arguments in vmod call".to_string(),
            ));
        }
        if slots[idx].is_some() {
            return Err(ast::VclError::new(
                span,
                format!("duplicate value for vmod argument slot {}", idx),
            ));
        }
        slots[idx] = Some(a.value);
    }

    let mut result: Vec<ast::Expr> = Vec::with_capacity(n);
    for (i, arg_spec) in sig.args.iter().enumerate() {
        let value = match slots[i].take() {
            None => ast::Expr::Omitted,
            Some(v) => {
                if arg_spec.optional {
                    let printed = printer::expr_to_string(&v);
                    let is_default = arg_spec
                        .default
                        .as_ref()
                        .map(|d| *d == printed)
                        .unwrap_or(false);
                    if is_default {
                        ast::Expr::Omitted
                    } else {
                        v
                    }
                } else {
                    v
                }
            }
        };
        result.push(value);
    }

    while matches!(result.last(), Some(ast::Expr::Omitted)) {
        result.pop();
    }

    *args = result
        .into_iter()
        .map(|value| ast::Arg { name: None, value })
        .collect();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::builder::*;
    use crate::vmod::ArgSpec;
    use pretty_assertions::assert_eq;

    fn log_spec() -> BTreeMap<String, VmodSpec> {
        let mut funcs = BTreeMap::new();
        funcs.insert(
            "log".to_string(),
            Sig {
                args: vec![
                    ArgSpec {
                        name: Some("msg".to_string()),
                        default: None,
                        optional: false,
                        enum_values: None,
                    },
                    ArgSpec {
                        name: Some("resolve".to_string()),
                        default: Some("false".to_string()),
                        optional: true,
                        enum_values: None,
                    },
                ],
            },
        );
        let mut specs = BTreeMap::new();
        specs.insert(
            "std".to_string(),
            VmodSpec {
                funcs,
                objects: BTreeMap::new(),
            },
        );
        specs
    }

    fn call_args(prog: &ast::Program) -> Vec<ast::Arg> {
        if let ast::Decl::Sub { body, .. } = &prog.decls[0] {
            if let ast::Stmt::Expr {
                expr: ast::Expr::Call { args, .. },
                ..
            } = &body[0]
            {
                return args.clone();
            }
        }
        panic!("expected a Call expr statement");
    }

    // N4: named -> positional reorder; omitted-optional and explicit-default
    // both -> Omitted; trailing Omitted truncated.
    #[test]
    fn n4_named_reorder_omitted_optional_truncated() {
        let mut prog = program(vec![sub(
            "vcl_recv",
            vec![expr_stmt(fcall(
                &["std", "log"],
                vec![narg("resolve", bool_(true)), narg("msg", str_("hi"))],
            ))],
        )]);
        let specs = log_spec();

        run(&mut prog, &specs).unwrap();

        let args = call_args(&prog);
        // resolve=true (not default) so it survives; reordered to declaration
        // order: [msg, resolve].
        assert_eq!(args.len(), 2);
        assert_eq!(args[0].name, None);
        assert!(matches!(&args[0].value, ast::Expr::Str(s) if s == "hi"));
        assert_eq!(args[1].name, None);
        assert!(matches!(args[1].value, ast::Expr::Bool(true)));
    }

    #[test]
    fn n4_omitted_optional_unfilled_truncated() {
        let mut prog = program(vec![sub(
            "vcl_recv",
            vec![expr_stmt(fcall(
                &["std", "log"],
                vec![narg("msg", str_("hi"))],
            ))],
        )]);
        let specs = log_spec();

        run(&mut prog, &specs).unwrap();

        let args = call_args(&prog);
        // resolve unfilled -> Omitted -> trailing truncated.
        assert_eq!(args.len(), 1);
        assert!(matches!(&args[0].value, ast::Expr::Str(s) if s == "hi"));
    }

    #[test]
    fn n4_explicit_default_value_truncated() {
        let mut prog = program(vec![sub(
            "vcl_recv",
            vec![expr_stmt(fcall(
                &["std", "log"],
                vec![arg(str_("hi")), narg("resolve", bool_(false))],
            ))],
        )]);
        let specs = log_spec();

        run(&mut prog, &specs).unwrap();

        let args = call_args(&prog);
        // resolve=false equals the spec default "false" -> Omitted -> truncated.
        assert_eq!(args.len(), 1);
        assert!(matches!(&args[0].value, ast::Expr::Str(s) if s == "hi"));
    }

    // N5: duplicate slot / unknown name errors; spec-less vmod calls untouched.
    #[test]
    fn n5_duplicate_slot_errors() {
        let mut prog = program(vec![sub(
            "vcl_recv",
            vec![expr_stmt(fcall(
                &["std", "log"],
                vec![arg(str_("hi")), narg("msg", str_("bye"))],
            ))],
        )]);
        let specs = log_spec();

        let err = run(&mut prog, &specs).unwrap_err();
        assert!(err.msg.contains("duplicate"), "got: {}", err.msg);
    }

    #[test]
    fn n5_unknown_named_arg_errors() {
        let mut prog = program(vec![sub(
            "vcl_recv",
            vec![expr_stmt(fcall(
                &["std", "log"],
                vec![narg("bogus", str_("hi"))],
            ))],
        )]);
        let specs = log_spec();

        let err = run(&mut prog, &specs).unwrap_err();
        assert!(err.msg.contains("unknown"), "got: {}", err.msg);
    }

    #[test]
    fn n5_spec_less_vmod_left_untouched() {
        let original_args = vec![arg(str_("hi")), narg("resolve", bool_(true))];
        let mut prog = program(vec![sub(
            "vcl_recv",
            vec![expr_stmt(fcall(&["other", "func"], original_args.clone()))],
        )]);
        let specs = log_spec(); // only knows "std", not "other"

        run(&mut prog, &specs).unwrap();

        let args = call_args(&prog);
        assert_eq!(args.len(), original_args.len());
        assert_eq!(args[0].name, original_args[0].name);
        assert_eq!(args[1].name, original_args[1].name);
        assert!(matches!(&args[0].value, ast::Expr::Str(s) if s == "hi"));
        assert!(matches!(args[1].value, ast::Expr::Bool(true)));
    }

    #[test]
    fn method_call_on_known_object_instance_rewritten() {
        // new obj = myvmod.ctor(); obj.method(msg="x");
        let mut methods = BTreeMap::new();
        methods.insert(
            "method".to_string(),
            Sig {
                args: vec![ArgSpec {
                    name: Some("msg".to_string()),
                    default: None,
                    optional: false,
                    enum_values: None,
                }],
            },
        );
        let mut objects = BTreeMap::new();
        objects.insert(
            "ctor".to_string(),
            crate::vmod::ObjSpec {
                init: Sig { args: vec![] },
                methods,
            },
        );
        let mut specs = BTreeMap::new();
        specs.insert(
            "myvmod".to_string(),
            VmodSpec {
                funcs: BTreeMap::new(),
                objects,
            },
        );

        let mut prog = program(vec![sub(
            "vcl_init",
            vec![
                new_("obj", "myvmod", "ctor", vec![]),
                expr_stmt(fcall(&["obj", "method"], vec![narg("msg", str_("hi"))])),
            ],
        )]);

        run(&mut prog, &specs).unwrap();

        if let ast::Decl::Sub { body, .. } = &prog.decls[0] {
            if let ast::Stmt::Expr {
                expr: ast::Expr::Call { args, .. },
                ..
            } = &body[1]
            {
                assert_eq!(args.len(), 1);
                assert_eq!(args[0].name, None);
                assert!(matches!(&args[0].value, ast::Expr::Str(s) if s == "hi"));
                return;
            }
        }
        panic!("expected method call rewritten");
    }
}
