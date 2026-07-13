//! Symbol table & validation pass (spec §5). Runs once over a parsed
//! `Program`, before normalization. Every error is a `VclError` with a span,
//! surfaced to the caller as exit code 2.
//!
//! Scope, per spec: this does *not* reimplement full VCC semantics
//! (return-action legality per builtin sub, variable read/write
//! permissions, type inference). `varnishd -C` remains the authoritative
//! compiler; this pass only catches name-resolution mistakes that would
//! otherwise silently confuse the normalizer/comparator.

use crate::ast::{self, Arg, Decl, Expr, Field, FieldValue, Program, Result, Span, Stmt, VclError};
use crate::vmod::{Sig, VmodSpec};
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Default)]
struct SymTab {
    backends: HashMap<String, Span>,
    probes: HashMap<String, Span>,
    acls: HashMap<String, Span>,
    /// Sub names seen so far. Redeclaring a *builtin* (`vcl_*`) name is
    /// legal (VCC concatenates their bodies in declaration order); a custom
    /// sub name may only be declared once, same as any other decl kind.
    subs: HashSet<String>,
    imports: HashMap<String, Span>,
    /// `new` instance name -> (vmod name, ctor/object-class name).
    objects: HashMap<String, (String, String)>,
}

/// Validate a parsed program's symbol references. `specs` holds loaded vmod
/// specs (only vmods actually found on disk / not skipped by `--no-vmod`
/// appear here) — vmods absent from this map are checked structurally only
/// (no signature errors are ever raised for them).
pub fn validate(p: &Program, specs: &BTreeMap<String, VmodSpec>) -> Result<()> {
    let mut sym = SymTab::default();

    // Phase 1: build the symbol table, checking for duplicate names within
    // each kind. Multiple `sub` blocks sharing a *builtin* (`vcl_*`) name are
    // legal (VCC concatenates them); redeclaring a custom sub name is a
    // duplicate-declaration error, exactly like any other decl kind (real
    // VCC rejects `sub helper {}` declared twice — confirmed against
    // `varnishd -C`).
    for d in &p.decls {
        match d {
            // `import` is idempotent -- unlike every other decl kind, real
            // VCC does not reject a repeated `import x;` (confirmed against
            // `varnishd -C`). This is the common case for real multi-file
            // VCL: several independently-written included files each
            // `import std;` for their own use.
            Decl::Import { name, span, .. } => {
                sym.imports.insert(name.clone(), *span);
            }
            Decl::Backend { name, span, .. } => {
                if sym.backends.contains_key(name) {
                    return Err(VclError::new(*span, format!("duplicate backend '{name}'")));
                }
                sym.backends.insert(name.clone(), *span);
            }
            Decl::Probe { name, span, .. } => {
                if sym.probes.contains_key(name) {
                    return Err(VclError::new(*span, format!("duplicate probe '{name}'")));
                }
                sym.probes.insert(name.clone(), *span);
            }
            Decl::Acl { name, span, .. } => {
                if sym.acls.contains_key(name) {
                    return Err(VclError::new(*span, format!("duplicate acl '{name}'")));
                }
                sym.acls.insert(name.clone(), *span);
            }
            Decl::Sub { name, span, .. } => {
                // The entire `vcl_*` namespace is reserved for hook
                // subroutines -- confirmed against `varnishd -C` ("the
                // names 'vcl_*' are reserved for subroutines"). We'd like
                // to flag a name outside the classic 14-hook set (a likely
                // typo, e.g. `vcl_devliver` for `vcl_deliver`), but the
                // full set of *valid* `vcl_*` names isn't enumerable by us
                // -- real Varnish Enterprise VCL legitimately declares
                // additional ones we have no way to know about (see
                // `ast::is_builtin_sub`'s doc comment). So this is a
                // non-fatal warning, once per distinct unrecognized name,
                // not an error.
                if name.starts_with("vcl_")
                    && !ast::BUILTIN_SUB_ORDER.contains(&name.as_str())
                    && !sym.subs.contains(name)
                {
                    eprintln!(
                        "warning: '{name}' is not a recognized builtin vcl_* subroutine name -- possible typo, or a vmod-specific hook this tool doesn't know about (treated as a hook: kept as-is, may be declared more than once)"
                    );
                }
                if !ast::is_builtin_sub(name) && sym.subs.contains(name) {
                    return Err(VclError::new(*span, format!("duplicate sub '{name}'")));
                }
                sym.subs.insert(name.clone());
            }
        }
    }

    // Phase 2: register every `new` statement declared inside a sub that's
    // transitively reachable from `vcl_init` via `call` (registration order
    // = declaration order, matching VCC's sequential construction), and
    // reject any `new` found anywhere else. Confirmed against `varnishd
    // -C`: `new` is legal not just literally inside `sub vcl_init`, but in
    // any custom sub only ever reached from vcl_init's call graph (a very
    // common real pattern: `vcl_init` calls a setup sub, which calls a
    // further helper sub that does the actual `new`). Doing this as its
    // own phase, ahead of general reference resolution, means method calls
    // on a vmod object are resolvable regardless of where in `Program.decls`
    // the constructing sub appears.
    let reachable = reachable_from_vcl_init(&p.decls);
    for d in &p.decls {
        if let Decl::Sub { name, body, .. } = d {
            if reachable.contains(name.as_str()) {
                collect_new_in_vcl_init(body, &mut sym, specs)?;
            } else {
                forbid_new(body)?;
            }
        }
    }

    // Phase 3: resolve every reference (call targets, `.probe =` refs, bare
    // symbol refs, vmod/method calls) and run vmod signature checks where a
    // spec is loaded.
    for d in &p.decls {
        match d {
            Decl::Backend {
                body: Some(fields), ..
            } => {
                for f in fields {
                    resolve_field(f, &sym, specs)?;
                }
            }
            Decl::Probe { body, .. } => {
                for f in body {
                    resolve_field(f, &sym, specs)?;
                }
            }
            Decl::Sub { body, .. } => {
                for s in body {
                    resolve_stmt(s, &sym, specs)?;
                }
            }
            _ => {}
        }
    }

    Ok(())
}

/// The set of sub names transitively reachable from `vcl_init` by following
/// `call` statements (including `vcl_init` itself). Same-named sub
/// fragments (not yet merged -- that's a normalize-pass concern, this runs
/// before normalization) are accumulated together, not overwritten, so a
/// call from any one fragment is seen.
fn reachable_from_vcl_init(decls: &[Decl]) -> HashSet<String> {
    let mut graph: HashMap<&str, Vec<&str>> = HashMap::new();
    for d in decls {
        if let Decl::Sub { name, body, .. } = d {
            let mut callees = Vec::new();
            collect_calls(body, &mut callees);
            graph.entry(name.as_str()).or_default().extend(callees);
        }
    }

    let mut reachable: HashSet<String> = HashSet::new();
    let mut stack = vec!["vcl_init"];
    while let Some(n) = stack.pop() {
        if reachable.insert(n.to_string()) {
            if let Some(callees) = graph.get(n) {
                for &c in callees {
                    if !reachable.contains(c) {
                        stack.push(c);
                    }
                }
            }
        }
    }
    reachable
}

/// Collects every `call`ed sub name inside `body`, descending into
/// `if`/`elsif`/`else`.
fn collect_calls<'a>(body: &'a [Stmt], out: &mut Vec<&'a str>) {
    for s in body {
        match s {
            Stmt::Call { sub, .. } => out.push(sub.as_str()),
            Stmt::If {
                arms, else_body, ..
            } => {
                for (_, b) in arms {
                    collect_calls(b, out);
                }
                if let Some(b) = else_body {
                    collect_calls(b, out);
                }
            }
            _ => {}
        }
    }
}

/// Recursively find `new` statements inside a body reachable from
/// `vcl_init` (descending into `if`/`elsif`/`else`), validating and
/// registering each one.
fn collect_new_in_vcl_init(
    body: &[Stmt],
    sym: &mut SymTab,
    specs: &BTreeMap<String, VmodSpec>,
) -> Result<()> {
    for s in body {
        match s {
            Stmt::New {
                name,
                vmod,
                ctor,
                args,
                span,
            } => {
                if !sym.imports.contains_key(vmod) {
                    return Err(VclError::new(
                        *span,
                        format!("new: vmod '{vmod}' is not imported"),
                    ));
                }
                if sym.objects.contains_key(name) {
                    return Err(VclError::new(
                        *span,
                        format!("duplicate vmod object name '{name}'"),
                    ));
                }
                match specs.get(vmod).and_then(|spec| spec.objects.get(ctor)) {
                    Some(objspec) => check_args(args, &objspec.init, *span, sym, specs)?,
                    None if specs.get(vmod).is_some() => {
                        return Err(VclError::new(
                            *span,
                            format!("unknown constructor '{vmod}.{ctor}'"),
                        ));
                    }
                    // vmod spec not loaded: structural, skip signature check
                    // but still resolve nested references in each arg.
                    None => {
                        for a in args {
                            resolve_expr(&a.value, *span, sym, specs)?;
                        }
                    }
                }
                sym.objects
                    .insert(name.clone(), (vmod.clone(), ctor.clone()));
            }
            Stmt::If {
                arms, else_body, ..
            } => {
                for (_, b) in arms {
                    collect_new_in_vcl_init(b, sym, specs)?;
                }
                if let Some(b) = else_body {
                    collect_new_in_vcl_init(b, sym, specs)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Recursively reject any `new` statement (used for every sub other than
/// `vcl_init`).
fn forbid_new(body: &[Stmt]) -> Result<()> {
    for s in body {
        match s {
            Stmt::New { span, .. } => {
                return Err(VclError::new(
                    *span,
                    "'new' is only allowed inside sub vcl_init",
                ));
            }
            Stmt::If {
                arms, else_body, ..
            } => {
                for (_, b) in arms {
                    forbid_new(b)?;
                }
                if let Some(b) = else_body {
                    forbid_new(b)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn resolve_field(f: &Field, sym: &SymTab, specs: &BTreeMap<String, VmodSpec>) -> Result<()> {
    match &f.value {
        FieldValue::Expr(e) => resolve_expr(e, f.span, sym, specs),
        FieldValue::Probe(fields) => {
            for inner in fields {
                resolve_field(inner, sym, specs)?;
            }
            Ok(())
        }
        FieldValue::ProbeRef(name) => {
            if sym.probes.contains_key(name) {
                Ok(())
            } else {
                Err(VclError::new(f.span, format!("undefined probe '{name}'")))
            }
        }
        FieldValue::StringList(_) => Ok(()),
    }
}

fn resolve_stmt(s: &Stmt, sym: &SymTab, specs: &BTreeMap<String, VmodSpec>) -> Result<()> {
    let ctx = s.span();
    match s {
        // `req.backend_hint`/`bereq.backend` are unambiguously backend-typed
        // -- no vmod-enum-constant ambiguity is possible here, unlike the
        // generic case in `resolve_expr`. Keep this one case strict.
        Stmt::Set { lhs, rhs, .. }
            if matches!(
                lhs.parts.last().map(String::as_str),
                Some("backend_hint" | "backend")
            ) =>
        {
            if let Expr::Var(parts) = rhs {
                if parts.len() == 1 && !sym.backends.contains_key(&parts[0]) {
                    return Err(VclError::new(
                        ctx,
                        format!("undefined reference '{}'", parts[0]),
                    ));
                }
            }
            resolve_expr(rhs, ctx, sym, specs)
        }
        Stmt::Set { rhs, .. } => resolve_expr(rhs, ctx, sym, specs),
        Stmt::Unset { .. } => Ok(()),
        Stmt::Call { sub, span } => {
            if ast::is_builtin_sub(sub) {
                return Err(VclError::new(
                    *span,
                    format!("cannot 'call' builtin sub '{sub}'"),
                ));
            }
            if !sym.subs.contains(sub) {
                return Err(VclError::new(*span, format!("undefined sub '{sub}'")));
            }
            Ok(())
        }
        Stmt::Return { action, .. } => {
            if let Some(a) = action {
                for e in &a.args {
                    resolve_expr(e, ctx, sym, specs)?;
                }
            }
            Ok(())
        }
        Stmt::Synthetic { value, .. } => resolve_expr(value, ctx, sym, specs),
        Stmt::If {
            arms, else_body, ..
        } => {
            for (cond, body) in arms {
                resolve_expr(cond, ctx, sym, specs)?;
                for st in body {
                    resolve_stmt(st, sym, specs)?;
                }
            }
            if let Some(b) = else_body {
                for st in b {
                    resolve_stmt(st, sym, specs)?;
                }
            }
            Ok(())
        }
        // Already fully handled in phase 2 (`collect_new_in_vcl_init`):
        // location, vmod import, ctor spec, AND every arg (name/arity/enum
        // checks + nested-reference resolution). Every `Stmt::New` reaching
        // phase 3 is guaranteed to be inside a vcl_init-reachable sub (a
        // non-reachable one would have already errored via `forbid_new`
        // before phase 3 ever runs) -- nothing left to do here.
        Stmt::New { .. } => Ok(()),
        Stmt::Expr { expr, .. } => resolve_expr(expr, ctx, sym, specs),
    }
}

/// Resolve references inside an expression tree. `ctx` is the span of the
/// nearest enclosing spanned node (`Expr` itself carries no span), used for
/// any error raised within.
fn resolve_expr(
    e: &Expr,
    ctx: Span,
    sym: &SymTab,
    specs: &BTreeMap<String, VmodSpec>,
) -> Result<()> {
    match e {
        // Bare single-part symbol. If it matches a declared backend/probe/
        // acl, it's a resolved reference to that. Otherwise: NOT an error
        // here -- a bare word in generic expression position (most
        // commonly a vmod-call argument, e.g. `scope = REQUEST`,
        // `ip_version = ipv4`, `log_level = MEDIUM`, `crypto.hmac_init
        // (sha256)`) is very often a vmod-defined enum constant, and which
        // constants are valid is defined per-vmod, per-parameter --
        // unenumerable by us (same reasoning as `ast::is_builtin_sub`'s
        // doc comment for `vcl_*` hook names). Just warn once so a real
        // typo doesn't go completely unnoticed. The one place a bare
        // backend-shaped reference is unambiguous -- `set req.backend_hint
        // = X;` / `set bereq.backend = X;` -- is checked strictly in
        // `resolve_stmt` instead, before this function ever sees it.
        Expr::Var(parts) if parts.len() == 1 => {
            let name = &parts[0];
            if !(sym.backends.contains_key(name)
                || sym.probes.contains_key(name)
                || sym.acls.contains_key(name))
            {
                eprintln!(
                    "warning: '{name}' does not match any declared backend/probe/acl -- possible typo, or a vmod-defined enum constant this tool doesn't know about"
                );
            }
            Ok(())
        }
        Expr::Var(_) => Ok(()),
        Expr::Call { target, args } => {
            // Resolved exactly once below, via `check_args` wherever a sig
            // is known (which also does the ENUM-value check), or via the
            // plain per-arg fallback otherwise (structural / no spec
            // loaded / single-part builtin target).
            let mut args_resolved = false;
            if target.len() >= 2 {
                let ns = &target[0];
                let member = &target[target.len() - 1];
                if let Some((vmod_name, ctor)) = sym.objects.get(ns) {
                    // `ns` is a vmod object instance (`new`'d).
                    if let Some(spec) = specs.get(vmod_name) {
                        if let Some(objspec) = spec.objects.get(ctor) {
                            match objspec.methods.get(member) {
                                Some(sig) => {
                                    check_args(args, sig, ctx, sym, specs)?;
                                    args_resolved = true;
                                }
                                None => {
                                    return Err(VclError::new(
                                        ctx,
                                        format!("unknown method '{member}' on object '{ns}'"),
                                    ))
                                }
                            }
                        }
                        // Object type absent from a loaded spec: shouldn't
                        // happen (would have failed at `new`-time), but
                        // skip silently rather than double-erroring.
                    }
                    // vmod spec not loaded: structural, skip signature check.
                } else if sym.imports.contains_key(ns) {
                    // `ns` is an imported vmod, `member` a plain function.
                    if let Some(spec) = specs.get(ns) {
                        match spec.funcs.get(member) {
                            Some(sig) => {
                                check_args(args, sig, ctx, sym, specs)?;
                                args_resolved = true;
                            }
                            None => {
                                return Err(VclError::new(
                                    ctx,
                                    format!("unknown function '{member}' in vmod '{ns}'"),
                                ))
                            }
                        }
                    }
                } else {
                    return Err(VclError::new(
                        ctx,
                        format!("call to unknown object or vmod '{ns}'"),
                    ));
                }
            }
            // Single-part call targets (e.g. `regsub(...)`) are core VCL
            // builtins, not vmod-namespaced -- nothing to look up above, so
            // `args_resolved` stays false and we fall back to the plain
            // per-arg resolver here (same for "spec not loaded" cases).
            if !args_resolved {
                for a in args {
                    resolve_expr(&a.value, ctx, sym, specs)?;
                }
            }
            Ok(())
        }
        Expr::Unary { expr, .. } => resolve_expr(expr, ctx, sym, specs),
        Expr::Binary { lhs, rhs, .. } => {
            resolve_expr(lhs, ctx, sym, specs)?;
            resolve_expr(rhs, ctx, sym, specs)
        }
        Expr::Str(_)
        | Expr::Num(_)
        | Expr::Duration(_)
        | Expr::Bytes(_)
        | Expr::Bool(_)
        | Expr::Omitted
        | Expr::CSource(_) => Ok(()),
    }
}

/// Named-arg and (lenient) arity checks shared by vmod function calls,
/// method calls, and `new` constructor calls, plus per-argument
/// resolution. General value type-checking is out of scope (`varnishd -C`
/// covers it); we only check:
/// - every named argument's name exists in the signature (strict);
/// - the number of *positional* arguments does not exceed the total number
///   of declared parameters (an unambiguous over-supply, regardless of how
///   many of those parameters are optional) -- under-supply is not
///   checked, since with optional args in play "how many are actually
///   required" isn't knowable without value-level defaults;
/// - for an argument landing in an `ENUM`-typed slot with a known value
///   list (straight from the vmod's JSON spec -- see `vmod::ArgSpec`):
///   the value, if a bare word, must be one of the legal literals. This
///   *is* an unambiguous, exhaustive check (unlike the generic bare-Var
///   case in `resolve_expr`), so it's a hard error, not a warning.
///
/// Every argument is resolved exactly once here (via this function or via
/// `resolve_expr` for non-enum slots) -- callers with a known `sig` should
/// call this instead of separately looping `resolve_expr` over `args`.
fn check_args(
    args: &[Arg],
    sig: &Sig,
    ctx: Span,
    sym: &SymTab,
    specs: &BTreeMap<String, VmodSpec>,
) -> Result<()> {
    for a in args {
        if let Some(n) = &a.name {
            if !sig
                .args
                .iter()
                .any(|s| s.name.as_deref() == Some(n.as_str()))
            {
                return Err(VclError::new(ctx, format!("unknown named argument '{n}'")));
            }
        }
    }
    let positional = args.iter().filter(|a| a.name.is_none()).count();
    if positional > sig.args.len() {
        return Err(VclError::new(ctx, "too many positional arguments"));
    }

    // Slot-map each arg (positional args fill slots left-to-right; named
    // args fill their named slot) to check ENUM values and to resolve
    // nested references exactly once.
    let mut next_pos = 0usize;
    for a in args {
        let slot = match &a.name {
            None => {
                let i = next_pos;
                next_pos += 1;
                sig.args.get(i)
            }
            Some(n) => sig
                .args
                .iter()
                .find(|s| s.name.as_deref() == Some(n.as_str())),
        };
        match (slot.and_then(|s| s.enum_values.as_ref()), &a.value) {
            (Some(values), Expr::Var(parts)) if parts.len() == 1 => {
                if !values.contains(&parts[0]) {
                    return Err(VclError::new(
                        ctx,
                        format!(
                            "'{}' is not a valid value for this enum argument (expected one of: {})",
                            parts[0],
                            values.join(", ")
                        ),
                    ));
                }
                // A known-valid enum literal -- do not also run it through
                // the generic bare-Var resolver, which would otherwise
                // warn (enum constants are never backend/probe/acl names).
            }
            _ => resolve_expr(&a.value, ctx, sym, specs)?,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_str;
    use crate::vmod::ArgSpec;
    use std::collections::BTreeMap;

    fn no_specs() -> BTreeMap<String, VmodSpec> {
        BTreeMap::new()
    }

    fn ok(src: &str) {
        let p = parse_str(src).expect("parse failed");
        validate(&p, &no_specs()).expect("expected validate() to succeed");
    }

    fn err(src: &str) -> String {
        let p = parse_str(src).expect("parse failed");
        let e = validate(&p, &no_specs()).expect_err("expected validate() to fail");
        e.msg
    }

    // S1: Duplicate backend/acl/probe name -> error; multiple same-named
    // `sub` blocks -> OK.
    #[test]
    fn s1_duplicate_names_error_same_name_subs_ok() {
        // Duplicate import: legal, no error (confirmed against `varnishd
        // -C`) -- unlike every other decl kind. Common in real multi-file
        // VCL: several included files each `import std;` independently.
        let dup_import = r#"
            vcl 4.1;
            import std;
            import std;
            sub vcl_recv { return (hash); }
        "#;
        ok(dup_import);

        let dup_backend = r#"
            vcl 4.1;
            backend b { .host = "1.2.3.4"; .port = "80"; }
            backend b { .host = "1.2.3.5"; .port = "80"; }
            sub vcl_recv { return (hash); }
        "#;
        assert!(err(dup_backend).contains("duplicate backend"));

        let dup_probe = r#"
            vcl 4.1;
            probe p { .url = "/"; }
            probe p { .url = "/x"; }
            sub vcl_recv { return (hash); }
        "#;
        assert!(err(dup_probe).contains("duplicate probe"));

        let dup_acl = r#"
            vcl 4.1;
            acl a { "127.0.0.1"; }
            acl a { "127.0.0.2"; }
            sub vcl_recv { return (hash); }
        "#;
        assert!(err(dup_acl).contains("duplicate acl"));

        // Multiple same-named *builtin* subs: legal, no error, bodies
        // effectively both present (validate() must walk both without
        // complaint) -- VCC concatenates them.
        let same_sub = r#"
            vcl 4.1;
            sub vcl_recv { set req.http.X-A = "1"; }
            sub vcl_recv { set req.http.X-B = "2"; }
        "#;
        ok(same_sub);

        // Redeclaring a *custom* sub name is a real VCC error (confirmed
        // against `varnishd -C`: "Subroutine 'helper' redefined") -- only
        // the fixed builtin hook names get the concatenation behavior.
        let dup_custom_sub = r#"
            vcl 4.1;
            sub helper { set req.http.a = "1"; }
            sub helper { set req.http.b = "2"; }
            sub vcl_recv { call helper; }
        "#;
        assert!(err(dup_custom_sub).contains("duplicate sub"));
    }

    // S1b: an unrecognized vcl_*-prefixed name (e.g. `vcl_devliver`, a typo
    // of `vcl_deliver`) is deliberately NOT a hard error -- only a
    // non-fatal warning (see the doc comment on this match arm and on
    // `ast::is_builtin_sub`: real Varnish Enterprise VCL legitimately uses
    // additional `vcl_*` hook names we cannot enumerate, so we can't
    // reliably distinguish a real hook from a typo).
    #[test]
    fn s1b_unrecognized_vcl_prefixed_sub_name_is_not_an_error() {
        let typo_builtin = r#"
            vcl 4.1;
            backend default none;
            sub vcl_recv { set req.http.x = "1"; }
            sub vcl_devliver { set resp.http.y = "1"; }
        "#;
        ok(typo_builtin);

        // A real builtin name is still fine, even declared just once.
        let real_builtin = r#"
            vcl 4.1;
            sub vcl_deliver { set resp.http.y = "1"; }
        "#;
        ok(real_builtin);

        // vcl_connect (Enterprise-specific, confirmed via real customer VCL
        // -- see ast::BUILTIN_SUB_ORDER's doc comment) must not false-positive
        // as an invalid vcl_*-prefixed name.
        let vcl_connect = r#"
            vcl 4.1;
            sub vcl_connect { return (accept); }
        "#;
        ok(vcl_connect);
    }

    // S2: undefined `call`, `.probe = missing`, bare-var `backend_hint` ->
    // error each.
    #[test]
    fn s2_undefined_references_error() {
        let bad_call = r#"
            vcl 4.1;
            sub vcl_recv { call missing; }
        "#;
        assert!(err(bad_call).contains("undefined sub"));

        let bad_probe_ref = r#"
            vcl 4.1;
            backend b {
                .host = "1.2.3.4";
                .port = "80";
                .probe = missing;
            }
            sub vcl_recv { return (hash); }
        "#;
        assert!(err(bad_probe_ref).contains("undefined probe"));

        let bad_bare_var = r#"
            vcl 4.1;
            backend b { .host = "1.2.3.4"; .port = "80"; }
            sub vcl_recv {
                set req.backend_hint = missing;
                return (hash);
            }
        "#;
        assert!(err(bad_bare_var).contains("undefined reference"));
    }

    // S3: `new` outside vcl_init -> error; inside -> registers VmodObject;
    // method call on unknown object -> error.
    #[test]
    fn s3_new_placement_and_object_resolution() {
        let new_outside_init = r#"
            vcl 4.1;
            import std;
            sub vcl_recv {
                new obj = std.foo();
                return (hash);
            }
        "#;
        assert!(err(new_outside_init).contains("vcl_init"));

        let new_inside_init_and_used = r#"
            vcl 4.1;
            import directors;
            backend web01 { .host = "1.2.3.4"; .port = "80"; }
            sub vcl_init {
                new vdir = directors.round_robin();
                vdir.add_backend(web01);
            }
            sub vcl_recv {
                set req.backend_hint = vdir.backend();
                return (hash);
            }
        "#;
        ok(new_inside_init_and_used);

        let unknown_object_method = r#"
            vcl 4.1;
            sub vcl_recv {
                set req.http.X = foo.bar();
                return (hash);
            }
        "#;
        assert!(err(unknown_object_method).contains("unknown object or vmod"));
    }

    // S3b: `new` is legal in a custom sub transitively reachable from
    // vcl_init via `call` (not just literally named "vcl_init") --
    // confirmed against `varnishd -C`: a real multi-file setup pattern is
    // vcl_init -> setup_sub -> helper_sub, each contributing `new` objects.
    // Still forbidden in a sub reachable only from a request-time hook.
    #[test]
    fn s3b_new_legal_transitively_reachable_from_vcl_init() {
        let transitively_reachable = r#"
            vcl 4.1;
            import directors;
            backend web01 { .host = "1.2.3.4"; .port = "80"; }
            sub vcl_init { call setup; }
            sub setup { call helper; }
            sub helper {
                new vdir = directors.round_robin();
                vdir.add_backend(web01);
            }
            sub vcl_recv {
                set req.backend_hint = vdir.backend();
                return (hash);
            }
        "#;
        ok(transitively_reachable);

        // A sub only reachable from vcl_recv (a request-time hook), even
        // transitively through another custom sub, still forbids `new`.
        let reachable_only_from_recv = r#"
            vcl 4.1;
            import directors;
            sub not_reachable_from_init { new vdir = directors.round_robin(); }
            sub vcl_recv { call not_reachable_from_init; return (hash); }
        "#;
        assert!(err(reachable_only_from_recv).contains("vcl_init"));
    }

    // S4: `call vcl_recv;` (a builtin) -> error.
    #[test]
    fn s4_call_builtin_is_error() {
        let src = r#"
            vcl 4.1;
            sub vcl_recv {
                call vcl_recv;
                return (hash);
            }
        "#;
        assert!(err(src).contains("builtin"));
    }

    // S5: with a fake loaded spec: unknown function, bad arity, unknown
    // named arg -> error each; value/type checks are NOT performed
    // (lenient).
    #[test]
    fn s5_vmod_signature_checks() {
        let mut specs = BTreeMap::new();
        specs.insert(
            "std".to_string(),
            VmodSpec {
                funcs: {
                    let mut m = BTreeMap::new();
                    m.insert(
                        "tolower".to_string(),
                        Sig {
                            args: vec![ArgSpec {
                                name: Some("s".to_string()),
                                default: None,
                                optional: false,
                                enum_values: None,
                            }],
                        },
                    );
                    m
                },
                objects: BTreeMap::new(),
            },
        );

        let parse_and_validate = |src: &str| -> Result<()> {
            let p = parse_str(src).unwrap();
            validate(&p, &specs)
        };

        // Unknown function.
        let unknown_func = r#"
            vcl 4.1;
            import std;
            sub vcl_recv {
                set req.http.X = std.nonexistent(req.url);
                return (hash);
            }
        "#;
        let e = parse_and_validate(unknown_func).expect_err("expected error");
        assert!(e.msg.contains("unknown function"), "{}", e.msg);

        // Bad arity: tolower takes exactly one slot; three positional args
        // can never be valid, regardless of optionality.
        let bad_arity = r#"
            vcl 4.1;
            import std;
            sub vcl_recv {
                set req.http.X = std.tolower(req.url, "a", "b");
                return (hash);
            }
        "#;
        let e = parse_and_validate(bad_arity).expect_err("expected error");
        assert!(e.msg.contains("too many"), "{}", e.msg);

        // Unknown named argument.
        let unknown_named = r#"
            vcl 4.1;
            import std;
            sub vcl_recv {
                set req.http.X = std.tolower(bogus = req.url);
                return (hash);
            }
        "#;
        let e = parse_and_validate(unknown_named).expect_err("expected error");
        assert!(e.msg.contains("unknown named argument"), "{}", e.msg);

        // Correct usage: one positional arg -> fine (value/type checks are
        // out of scope; a non-STRING arg like a bool wouldn't be flagged
        // either, and that's intentional).
        let ok_call = r#"
            vcl 4.1;
            import std;
            sub vcl_recv {
                set req.http.X = std.tolower(req.url);
                return (hash);
            }
        "#;
        parse_and_validate(ok_call).expect("expected success");
    }

    // S5b: ENUM-typed vmod arguments are validated against the legal value
    // list straight from the JSON spec (real shape, from libvmod_blob's
    // actual `encode` function) -- a valid literal is silent, an invalid
    // one is a hard error (unlike the generic bare-Var case, this is
    // unambiguous: the vmod spec exhaustively lists what's legal).
    #[test]
    fn s5b_enum_argument_values_checked_against_spec() {
        let mut specs = BTreeMap::new();
        specs.insert(
            "blob".to_string(),
            VmodSpec {
                funcs: {
                    let mut m = BTreeMap::new();
                    m.insert(
                        "encode".to_string(),
                        Sig {
                            args: vec![
                                ArgSpec {
                                    name: Some("encoding".to_string()),
                                    default: Some("IDENTITY".to_string()),
                                    optional: true,
                                    enum_values: Some(
                                        ["IDENTITY", "BASE64", "BASE64URL", "HEX", "URL"]
                                            .iter()
                                            .map(|s| s.to_string())
                                            .collect(),
                                    ),
                                },
                                ArgSpec {
                                    name: Some("blob".to_string()),
                                    default: None,
                                    optional: false,
                                    enum_values: None,
                                },
                            ],
                        },
                    );
                    m
                },
                objects: BTreeMap::new(),
            },
        );

        let parse_and_validate = |src: &str| -> Result<()> {
            let p = parse_str(src).unwrap();
            validate(&p, &specs)
        };

        let valid_enum = r#"
            vcl 4.1;
            import blob;
            sub vcl_recv {
                set req.http.X = blob.encode(encoding = HEX, blob = req.hash);
                return (hash);
            }
        "#;
        parse_and_validate(valid_enum).expect("HEX is a legal encoding value");

        let invalid_enum = r#"
            vcl 4.1;
            import blob;
            sub vcl_recv {
                set req.http.X = blob.encode(encoding = BOGUS, blob = req.hash);
                return (hash);
            }
        "#;
        let e = parse_and_validate(invalid_enum).expect_err("BOGUS is not a legal encoding value");
        assert!(e.msg.contains("BOGUS"), "{}", e.msg);
        assert!(e.msg.contains("not a valid value"), "{}", e.msg);
    }
}
