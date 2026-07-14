//! Normalize pass 4 — canonical renaming (`src/normalize/rename.rs`) — the heart.
//!
//! Renames every user-defined name to a positional canonical name, anchored by
//! *usage order*, so renaming is independent of declaration order and of
//! original spelling. See spec section 7, pass 4, for the full algorithm.
//!
//! Renameable kinds and canonical patterns: backends → `backend_N`, probes →
//! `probe_N`, ACLs → `acl_N`, custom subs → `sub_N`, vmod object instances
//! (`new`) → `obj_N`. Exceptions that keep their names: builtin `vcl_*` subs;
//! a backend literally named `default`.

use crate::ast::{self, Arg, Decl, Expr, FieldValue, NameMap, Program, Stmt};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Kind {
    Backend,
    Probe,
    Acl,
    Sub,
    Obj,
}

impl Kind {
    fn prefix(self) -> &'static str {
        match self {
            Kind::Backend => "backend",
            Kind::Probe => "probe",
            Kind::Acl => "acl",
            Kind::Sub => "sub",
            Kind::Obj => "obj",
        }
    }
}

/// Mutable renaming state threaded through the whole algorithm.
struct Ctx {
    /// Declared name → kind, for top-level Backend/Probe/Acl/Sub decls (built
    /// once from the *original* names before any renaming happens).
    decl_kind: HashMap<String, Kind>,
    /// Names introduced by `new name = vmod.ctor(...)` statements.
    obj_instances: HashSet<String>,
    /// (kind, original name) → canonical name, assigned as usage is discovered.
    assigned: HashMap<(Kind, String), String>,
    counters: HashMap<Kind, u32>,
    visited_subs: HashSet<String>,
    /// (kind label, canonical, original) in assignment order — becomes the NameMap.
    order: Vec<(String, String, String)>,
}

impl Ctx {
    /// Assigns (or returns the existing) canonical name for `(kind, name)`.
    fn assign(&mut self, kind: Kind, name: &str) -> String {
        if let Some(c) = self.assigned.get(&(kind, name.to_string())) {
            return c.clone();
        }
        let keep = (kind == Kind::Sub && ast::is_builtin_sub(name))
            || (kind == Kind::Backend && name == "default");
        let canonical = if keep {
            name.to_string()
        } else {
            let counter = self.counters.entry(kind).or_insert(0);
            *counter += 1;
            format!("{}_{}", kind.prefix(), counter)
        };
        self.assigned
            .insert((kind, name.to_string()), canonical.clone());
        if !keep {
            self.order.push((
                kind.prefix().to_string(),
                canonical.clone(),
                name.to_string(),
            ));
        }
        canonical
    }

    /// Looks up the canonical name for a bare identifier, if it refers to a
    /// known (already-assigned) object/decl. Returns `None` for anything else
    /// (builtin scope vars, vmod names, function names, ...).
    fn resolve(&self, name: &str) -> Option<String> {
        if self.obj_instances.contains(name) {
            self.assigned.get(&(Kind::Obj, name.to_string())).cloned()
        } else if let Some(&kind) = self.decl_kind.get(name) {
            self.assigned.get(&(kind, name.to_string())).cloned()
        } else {
            None
        }
    }
}

/// Step 1: merge multiple same-named *builtin* `sub` blocks (concatenate
/// bodies in declaration order; keep the first decl's position/span).
///
/// Only builtin (`vcl_*`) names get this treatment -- that is a real,
/// specific VCL feature (VCC auto-chains the fixed hook-point subs). A
/// duplicate *custom* sub name is invalid VCL (`varnishd -C` rejects it:
/// "Subroutine 'x' redefined") and should already have been caught by
/// `symbols::validate` before normalization runs; this function leaves any
/// such duplicate as separate, unmerged decls rather than silently
/// concatenating them, so it stays correct even if called directly (e.g.
/// from a unit test) without going through validation first.
fn merge_subs(p: &mut Program) {
    let mut first_idx: HashMap<String, usize> = HashMap::new();
    let mut merged: Vec<Decl> = Vec::new();
    for decl in std::mem::take(&mut p.decls) {
        if let Decl::Sub { name, body, span } = decl {
            if ast::is_builtin_sub(&name) {
                if let Some(&idx) = first_idx.get(&name) {
                    reattach_fragment_comments(p, span, &body);
                    if let Decl::Sub { body: existing, .. } = &mut merged[idx] {
                        existing.extend(body);
                    }
                } else {
                    first_idx.insert(name.clone(), merged.len());
                    merged.push(Decl::Sub { name, body, span });
                }
            } else {
                merged.push(Decl::Sub { name, body, span });
            }
        } else {
            merged.push(decl);
        }
    }
    p.decls = merged;
}

/// A merged-away builtin-sub fragment's own (former Decl-level) leading and
/// trailing comments have no home once the fragment disappears -- reattach
/// them as a leading, unindented comment on the first statement it
/// contributes (the position that used to be "the start of this fragment's
/// block" now sits mid-body of the merged sub). If the fragment's body is
/// empty, there's nothing to attach them to and they're dropped.
///
/// The fragment's own orphan `after` comments (right before its own closing
/// `}`) need no action here: they're already attached to the last stmt of
/// `body`, which is about to become a mid-body stmt of the merged sub --
/// exactly where they belong positionally.
fn reattach_fragment_comments(p: &mut Program, fragment_span: ast::Span, body: &[Stmt]) {
    let Some(frag_comments) = p.comments.take(fragment_span) else {
        return;
    };
    let mut reattached: Vec<ast::LeadingComment> = frag_comments
        .leading
        .into_iter()
        .map(|mut c| {
            c.unindented = true;
            c
        })
        .collect();
    if let Some(text) = frag_comments.trailing {
        reattached.push(ast::LeadingComment {
            text,
            unindented: true,
        });
    }
    if reattached.is_empty() {
        return;
    }
    if let Some(first_stmt) = body.first() {
        let entry = p.comments.entry(first_stmt.span());
        reattached.extend(std::mem::take(&mut entry.leading));
        entry.leading = reattached;
    }
}

fn build_decl_kind(p: &Program) -> HashMap<String, Kind> {
    let mut m = HashMap::new();
    for d in &p.decls {
        match d {
            Decl::Backend { name, .. } => {
                m.insert(name.clone(), Kind::Backend);
            }
            Decl::Probe { name, .. } => {
                m.insert(name.clone(), Kind::Probe);
            }
            Decl::Acl { name, .. } => {
                m.insert(name.clone(), Kind::Acl);
            }
            Decl::Sub { name, .. } => {
                m.insert(name.clone(), Kind::Sub);
            }
            Decl::Import { .. } => {}
        }
    }
    m
}

fn decls_index(p: &Program) -> HashMap<String, usize> {
    let mut m = HashMap::new();
    for (i, d) in p.decls.iter().enumerate() {
        if !matches!(d, Decl::Import { .. }) {
            m.insert(d.name().to_string(), i);
        }
    }
    m
}

/// Assigns `(kind, name)` and, on first assignment, cascades into whatever
/// the object references (a backend's `.probe`, a sub's body via DFS).
fn touch(
    ctx: &mut Ctx,
    p: &Program,
    idx: &HashMap<String, usize>,
    kind: Kind,
    name: &str,
) -> String {
    let already = ctx.assigned.contains_key(&(kind, name.to_string()));
    let canonical = ctx.assign(kind, name);
    if already {
        return canonical;
    }
    match kind {
        Kind::Backend => {
            if let Some(&i) = idx.get(name) {
                if let Decl::Backend {
                    body: Some(fields), ..
                } = &p.decls[i]
                {
                    for f in fields {
                        if f.name == "probe" {
                            if let FieldValue::ProbeRef(pname) = &f.value {
                                touch(ctx, p, idx, Kind::Probe, pname);
                            }
                        }
                    }
                }
            }
        }
        Kind::Sub if !ctx.visited_subs.contains(name) => {
            ctx.visited_subs.insert(name.to_string());
            if let Some(&i) = idx.get(name) {
                if let Decl::Sub { body, .. } = &p.decls[i] {
                    walk_stmts(ctx, p, idx, body);
                }
            }
        }
        _ => {}
    }
    canonical
}

fn touch_ref(ctx: &mut Ctx, p: &Program, idx: &HashMap<String, usize>, name: &str) {
    if ctx.obj_instances.contains(name) {
        touch(ctx, p, idx, Kind::Obj, name);
    } else if let Some(kind) = ctx.decl_kind.get(name).copied() {
        touch(ctx, p, idx, kind, name);
    }
    // Otherwise: not a known user object (builtin scope var, vmod name,
    // builtin function name, ...) — leave untouched.
}

fn walk_stmts(ctx: &mut Ctx, p: &Program, idx: &HashMap<String, usize>, stmts: &[Stmt]) {
    for s in stmts {
        walk_stmt(ctx, p, idx, s);
    }
}

fn walk_stmt(ctx: &mut Ctx, p: &Program, idx: &HashMap<String, usize>, s: &Stmt) {
    match s {
        Stmt::Set { rhs, .. } => walk_expr(ctx, p, idx, rhs),
        Stmt::Unset { .. } => {}
        Stmt::Call { sub, .. } => {
            touch(ctx, p, idx, Kind::Sub, sub);
        }
        Stmt::Return { action, .. } => {
            if let Some(a) = action {
                for e in &a.args {
                    walk_expr(ctx, p, idx, e);
                }
            }
        }
        Stmt::Synthetic { value, .. } => walk_expr(ctx, p, idx, value),
        Stmt::If {
            arms, else_body, ..
        } => {
            for (cond, body) in arms {
                walk_expr(ctx, p, idx, cond);
                walk_stmts(ctx, p, idx, body);
            }
            if let Some(eb) = else_body {
                walk_stmts(ctx, p, idx, eb);
            }
        }
        Stmt::New { name, args, .. } => {
            ctx.obj_instances.insert(name.clone());
            ctx.assign(Kind::Obj, name);
            for a in args {
                walk_expr(ctx, p, idx, &a.value);
            }
        }
        Stmt::Expr { expr, .. } => walk_expr(ctx, p, idx, expr),
    }
}

fn walk_expr(ctx: &mut Ctx, p: &Program, idx: &HashMap<String, usize>, e: &Expr) {
    match e {
        Expr::Str(_)
        | Expr::Num(_)
        | Expr::Duration(_)
        | Expr::Bytes(_)
        | Expr::Bool(_)
        | Expr::Omitted
        | Expr::CSource(_) => {}
        Expr::Var(parts) => {
            if let Some(first) = parts.first() {
                touch_ref(ctx, p, idx, first);
            }
        }
        Expr::Call { target, args } => {
            if let Some(first) = target.first() {
                touch_ref(ctx, p, idx, first);
            }
            for a in args {
                walk_expr(ctx, p, idx, &a.value);
            }
        }
        Expr::Unary { expr, .. } => walk_expr(ctx, p, idx, expr),
        Expr::Binary { lhs, rhs, .. } => {
            walk_expr(ctx, p, idx, lhs);
            walk_expr(ctx, p, idx, rhs);
        }
    }
}

/// Step 2/3: DFS the builtin subs that exist, in `ast::BUILTIN_SUB_ORDER`,
/// then any *other* `vcl_*`-prefixed sub present (in declaration order) --
/// a name outside the fixed list is still a real, dispatched hook as far as
/// `ast::is_builtin_sub` is concerned (see its doc comment: the full set of
/// valid names isn't enumerable by us), so it must still be walked for
/// usage-order DFS anchoring. Without this, custom subs called *only* from
/// such an unknown hook would incorrectly fall into the dead-tiebreak pool
/// instead of being anchored by where they're actually used.
fn walk_builtins(ctx: &mut Ctx, p: &Program, idx: &HashMap<String, usize>) {
    for &bname in ast::BUILTIN_SUB_ORDER {
        if let Some(&i) = idx.get(bname) {
            if let Decl::Sub { body, .. } = &p.decls[i] {
                ctx.visited_subs.insert(bname.to_string());
                ctx.assign(Kind::Sub, bname);
                walk_stmts(ctx, p, idx, body);
            }
        }
    }
    for d in &p.decls {
        if let Decl::Sub { name, body, .. } = d {
            if ast::is_builtin_sub(name)
                && !ast::BUILTIN_SUB_ORDER.contains(&name.as_str())
                && !ctx.visited_subs.contains(name)
            {
                ctx.visited_subs.insert(name.clone());
                ctx.assign(Kind::Sub, name);
                walk_stmts(ctx, p, idx, body);
            }
        }
    }
}

/// Ensures a backend literally named `default` keeps its name even if dead
/// (never referenced) — the exception applies unconditionally, not just when
/// the backend happens to be used.
fn ensure_default_backend(ctx: &mut Ctx, p: &Program, idx: &HashMap<String, usize>) {
    for d in &p.decls {
        if let Decl::Backend { name, .. } = d {
            if name == "default" && !ctx.assigned.contains_key(&(Kind::Backend, name.clone())) {
                touch(ctx, p, idx, Kind::Backend, name);
            }
        }
    }
}

/// Step 4: assign canonical names to remaining (dead/unreferenced) decls of
/// `kind`, tie-broken by lexicographic order of their canonical JSON with
/// their own name masked to `$self` and not-yet-assigned names left as-is.
fn assign_dead(ctx: &mut Ctx, p: &Program, kind: Kind) {
    let is_default_backend =
        |d: &Decl| matches!(d, Decl::Backend { name, .. } if name == "default");
    let mut candidates: Vec<(String, &Decl)> = Vec::new();
    for d in &p.decls {
        let matches_kind = match d {
            Decl::Backend { .. } => kind == Kind::Backend && !is_default_backend(d),
            Decl::Probe { .. } => kind == Kind::Probe,
            Decl::Acl { .. } => kind == Kind::Acl,
            Decl::Sub { name, .. } => kind == Kind::Sub && !ast::is_builtin_sub(name),
            Decl::Import { .. } => false,
        };
        if !matches_kind {
            continue;
        }
        let name = d.name().to_string();
        if !ctx.assigned.contains_key(&(kind, name.clone())) {
            candidates.push((name, d));
        }
    }

    let mut scored: Vec<(String, String)> = candidates
        .into_iter()
        .map(|(name, d)| {
            let mut clone = d.clone();
            let self_name = name.clone();
            let f = |n: &str| -> String {
                if n == self_name {
                    "$self".to_string()
                } else if let Some(c) = ctx.resolve(n) {
                    c
                } else {
                    n.to_string()
                }
            };
            rename_decl_with(&mut clone, &f);
            let score =
                serde_json::to_string(&clone).expect("Decl serialization should never fail");
            (score, name)
        })
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0));

    for (_, name) in scored {
        ctx.assign(kind, &name);
    }
}

/// Generic tree-rename applier, parameterized on the identifier transform
/// `f`. Used both for the final apply pass (`f` resolves every name to its
/// final canonical form) and for dead-decl scoring (`f` masks self-references
/// to `$self` and leaves not-yet-assigned names untouched).
fn rename_decl_with(decl: &mut Decl, f: &impl Fn(&str) -> String) {
    match decl {
        Decl::Backend { name, body, .. } => {
            *name = f(name);
            if let Some(fields) = body {
                for field in fields {
                    if field.name == "probe" {
                        if let FieldValue::ProbeRef(p) = &mut field.value {
                            *p = f(p);
                        }
                    }
                }
            }
        }
        Decl::Probe { name, .. } => {
            *name = f(name);
        }
        Decl::Acl { name, .. } => {
            *name = f(name);
        }
        Decl::Sub { name, body, .. } => {
            *name = f(name);
            for s in body {
                rename_stmt_with(s, f);
            }
        }
        Decl::Import { .. } => {}
    }
}

fn rename_stmt_with(s: &mut Stmt, f: &impl Fn(&str) -> String) {
    match s {
        Stmt::Set { rhs, .. } => rename_expr_with(rhs, f),
        Stmt::Unset { .. } => {}
        Stmt::Call { sub, .. } => *sub = f(sub),
        Stmt::Return { action, .. } => {
            if let Some(a) = action {
                for e in &mut a.args {
                    rename_expr_with(e, f);
                }
            }
        }
        Stmt::Synthetic { value, .. } => rename_expr_with(value, f),
        Stmt::If {
            arms, else_body, ..
        } => {
            for (cond, body) in arms {
                rename_expr_with(cond, f);
                for s in body {
                    rename_stmt_with(s, f);
                }
            }
            if let Some(eb) = else_body {
                for s in eb {
                    rename_stmt_with(s, f);
                }
            }
        }
        Stmt::New { name, args, .. } => {
            *name = f(name);
            for a in args {
                rename_arg_with(a, f);
            }
        }
        Stmt::Expr { expr, .. } => rename_expr_with(expr, f),
    }
}

fn rename_arg_with(a: &mut Arg, f: &impl Fn(&str) -> String) {
    rename_expr_with(&mut a.value, f);
}

fn rename_expr_with(e: &mut Expr, f: &impl Fn(&str) -> String) {
    match e {
        Expr::Str(_)
        | Expr::Num(_)
        | Expr::Duration(_)
        | Expr::Bytes(_)
        | Expr::Bool(_)
        | Expr::Omitted
        | Expr::CSource(_) => {}
        Expr::Var(parts) => {
            if let Some(first) = parts.first_mut() {
                *first = f(first);
            }
        }
        Expr::Call { target, args } => {
            if let Some(first) = target.first_mut() {
                *first = f(first);
            }
            for a in args {
                rename_arg_with(a, f);
            }
        }
        Expr::Unary { expr, .. } => rename_expr_with(expr, f),
        Expr::Binary { lhs, rhs, .. } => {
            rename_expr_with(lhs, f);
            rename_expr_with(rhs, f);
        }
    }
}

/// Merges same-named builtin `sub` fragments and finalizes internal
/// placeholder names, without renaming anything else. Used when the rest
/// of pass 4's renaming is skipped (e.g. `vcl-normalizer print` without
/// `--rename`): fragments must still be concatenated, and placeholders
/// must still become valid identifiers, to reflect the program VCC would
/// actually run -- everything else (real, user-declared names) is left
/// untouched.
pub fn merge_only(p: &mut Program) {
    merge_subs(p);
    finalize_anonymous_names(p);
}

/// Rewrites internal placeholder names (currently only `$anon_probe_N`,
/// from inline-probe lifting in normalize pass 3) to valid, stable
/// identifiers (`probe_1`, `probe_2`, ...), without touching any other
/// (real, user-declared) name. `$`-prefixed names are never valid VCL
/// identifiers and must not leak into printed output.
fn finalize_anonymous_names(p: &mut Program) {
    let mut counters: HashMap<Kind, u32> = HashMap::new();
    let mut map: HashMap<String, String> = HashMap::new();

    for d in &p.decls {
        let name = d.name();
        if !name.starts_with('$') {
            continue;
        }
        let kind = match d {
            Decl::Backend { .. } => Kind::Backend,
            Decl::Probe { .. } => Kind::Probe,
            Decl::Acl { .. } => Kind::Acl,
            Decl::Sub { .. } => Kind::Sub,
            Decl::Import { .. } => continue,
        };
        let counter = counters.entry(kind).or_insert(0);
        *counter += 1;
        map.insert(name.to_string(), format!("{}_{}", kind.prefix(), counter));
    }

    if map.is_empty() {
        return;
    }
    let f = |n: &str| -> String { map.get(n).cloned().unwrap_or_else(|| n.to_string()) };
    for d in &mut p.decls {
        rename_decl_with(d, &f);
    }
}

/// Runs normalize pass 4 (canonical renaming) on `p` in place, returning the
/// `canonical → original` bijection for `--names` output.
pub fn run(p: &mut Program) -> NameMap {
    merge_subs(p);

    let decl_kind = build_decl_kind(p);
    let idx = decls_index(p);
    let mut ctx = Ctx {
        decl_kind,
        obj_instances: HashSet::new(),
        assigned: HashMap::new(),
        counters: HashMap::new(),
        visited_subs: HashSet::new(),
        order: Vec::new(),
    };

    walk_builtins(&mut ctx, p, &idx);

    // Dead (unreferenced) decls, dependency order: probes, backends, acls, subs.
    assign_dead(&mut ctx, p, Kind::Probe);
    ensure_default_backend(&mut ctx, p, &idx);
    assign_dead(&mut ctx, p, Kind::Backend);
    assign_dead(&mut ctx, p, Kind::Acl);
    assign_dead(&mut ctx, p, Kind::Sub);

    // Apply the fully-resolved assignment table everywhere.
    let f = |n: &str| -> String { ctx.resolve(n).unwrap_or_else(|| n.to_string()) };
    for d in &mut p.decls {
        rename_decl_with(d, &f);
    }

    NameMap { entries: ctx.order }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::builder::*;
    use crate::ast::FieldValue;
    use pretty_assertions::assert_eq;

    #[test]
    fn n7_shuffled_decls_produce_identical_canonical_ast() {
        let decls_a = vec![
            backend("web_a", vec![fexpr("host", str_("a.example.com"))]),
            backend("web_b", vec![fexpr("host", str_("b.example.com"))]),
            acl("internal", vec![acl_entry("10.0.0.0", Some(8), false)]),
            sub(
                "vcl_recv",
                vec![
                    set(&["req", "http", "x"], var(&["web_a"])),
                    if_(
                        vec![(
                            bin(
                                ast::BinOp::Match,
                                var(&["client", "ip"]),
                                var(&["internal"]),
                            ),
                            vec![call("helper")],
                        )],
                        None,
                    ),
                ],
            ),
            sub("helper", vec![set(&["req", "http", "y"], var(&["web_b"]))]),
        ];
        // Shuffled order.
        let decls_b = vec![
            decls_a[3].clone(),
            decls_a[4].clone(),
            decls_a[1].clone(),
            decls_a[0].clone(),
            decls_a[2].clone(),
        ];

        let mut pa = program(decls_a);
        let mut pb = program(decls_b);

        run(&mut pa);
        run(&mut pb);
        crate::normalize::sort::run(&mut pa);
        crate::normalize::sort::run(&mut pb);

        let ja = serde_json::to_value(&pa.decls).unwrap();
        let jb = serde_json::to_value(&pb.decls).unwrap();
        assert_eq!(ja, jb);
    }

    #[test]
    fn n7b_interleaved_same_name_subs_merge_in_relative_order_regardless_of_interleaving() {
        // sub vcl_recv { X } / sub vcl_deliver { Y } / sub vcl_recv { Z }
        let decls_a = vec![
            sub("vcl_recv", vec![set(&["req", "http", "x"], str_("X"))]),
            sub("vcl_deliver", vec![set(&["resp", "http", "y"], str_("Y"))]),
            sub("vcl_recv", vec![set(&["req", "http", "z"], str_("Z"))]),
        ];
        // Same fragments, but vcl_deliver now precedes both vcl_recv fragments.
        let decls_b = vec![
            sub("vcl_deliver", vec![set(&["resp", "http", "y"], str_("Y"))]),
            sub("vcl_recv", vec![set(&["req", "http", "x"], str_("X"))]),
            sub("vcl_recv", vec![set(&["req", "http", "z"], str_("Z"))]),
        ];

        let mut pa = program(decls_a);
        let mut pb = program(decls_b);

        run(&mut pa);
        run(&mut pb);
        crate::normalize::sort::run(&mut pa);
        crate::normalize::sort::run(&mut pb);

        // Both merge to one vcl_recv body [X, Z] and one vcl_deliver body [Y] —
        // interleaving a different-named sub between the two vcl_recv fragments
        // must not change the merged (concatenated) order, and top-level sort
        // makes the surrounding decl order a non-difference.
        let ja = serde_json::to_value(&pa.decls).unwrap();
        let jb = serde_json::to_value(&pb.decls).unwrap();
        assert_eq!(ja, jb);

        let recv = pa
            .decls
            .iter()
            .find(|d| d.name() == "vcl_recv")
            .expect("vcl_recv present");
        if let Decl::Sub { body, .. } = recv {
            assert_eq!(body.len(), 2, "expected merged 2-statement body");
        } else {
            panic!("expected Decl::Sub");
        }
    }

    // NOTE: an earlier version of this test file had n7c/n7d exercise a
    // *custom* sub split into multiple `sub name { ... }` fragments. That
    // scenario is not valid VCL: confirmed against real `varnishd -C`, only
    // builtin (`vcl_*`) sub names may be redeclared like this (VCC's
    // documented hook-chaining feature) -- redeclaring a custom sub name is
    // a compile error ("Subroutine 'x' redefined"), enforced by
    // `symbols::validate` (see its `s1_duplicate_names_error_same_name_subs_ok`
    // test). `merge_subs` itself now only merges builtin-named fragments
    // (see its doc comment) and leaves duplicate custom names as separate,
    // unmerged decls -- exercised directly below without going through
    // `validate` first, since `merge_subs` must stay correct even when
    // called on input that hasn't been validated.

    #[test]
    fn merge_only_keeps_real_names_but_finalizes_anonymous_probe() {
        // Mirrors the real pipeline with rename disabled (`vcl-normalizer print`
        // without `--rename`): pass 3 (probes::run) lifts an inline probe
        // to a `$anon_probe_1` placeholder decl first; merge_only must turn
        // that into a valid identifier ("probe_1") without touching any of
        // the real, user-declared names (web01, vcl_recv, ...).
        let mut p = program(vec![
            backend(
                "web01",
                vec![
                    fexpr("host", str_("1.2.3.4")),
                    field(
                        "probe",
                        FieldValue::Probe(vec![fexpr("url", str_("/health"))]),
                    ),
                ],
            ),
            sub(
                "vcl_recv",
                vec![set(&["req", "http", "x"], var(&["web01"]))],
            ),
        ]);
        crate::normalize::probes::run(&mut p);

        // Sanity: the placeholder exists before merge_only runs.
        assert!(p.decls.iter().any(|d| d.name() == "$anon_probe_1"));

        merge_only(&mut p);

        assert!(
            !p.decls.iter().any(|d| d.name().starts_with('$')),
            "no placeholder name should survive merge_only: {:?}",
            p.decls.iter().map(Decl::name).collect::<Vec<_>>()
        );
        assert!(p.decls.iter().any(|d| d.name() == "probe_1"));
        // Real, user-declared names are untouched.
        assert!(p.decls.iter().any(|d| d.name() == "web01"));
        assert!(p.decls.iter().any(|d| d.name() == "vcl_recv"));

        let backend = p
            .decls
            .iter()
            .find(|d| d.name() == "web01")
            .expect("web01 present");
        if let Decl::Backend {
            body: Some(fields), ..
        } = backend
        {
            let probe_field = fields.iter().find(|f| f.name == "probe").unwrap();
            match &probe_field.value {
                FieldValue::ProbeRef(name) => assert_eq!(name, "probe_1"),
                other => panic!("expected ProbeRef, got {other:?}"),
            }
        } else {
            panic!("expected Decl::Backend with a body");
        }
    }

    #[test]
    fn n7c_merge_subs_only_merges_builtin_names_leaves_custom_duplicates_unmerged() {
        let mut p = program(vec![
            sub("vcl_recv", vec![set(&["req", "http", "a"], str_("A"))]),
            sub("helper", vec![set(&["req", "http", "b"], str_("B"))]),
            sub("vcl_recv", vec![set(&["req", "http", "c"], str_("C"))]),
            sub("helper", vec![set(&["req", "http", "d"], str_("D"))]),
        ]);

        merge_subs(&mut p);

        let recv_count = p
            .decls
            .iter()
            .filter(|d| matches!(d, Decl::Sub{name, ..} if name == "vcl_recv"))
            .count();
        assert_eq!(recv_count, 1, "builtin fragments merge into one decl");
        let recv = p
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Sub{name, ..} if name == "vcl_recv"))
            .unwrap();
        if let Decl::Sub { body, .. } = recv {
            assert_eq!(body.len(), 2, "merged vcl_recv body is [A, C]");
        }

        let helper_count = p
            .decls
            .iter()
            .filter(|d| matches!(d, Decl::Sub{name, ..} if name == "helper"))
            .count();
        assert_eq!(
            helper_count, 2,
            "duplicate custom sub fragments are left separate, not merged"
        );
    }

    #[test]
    fn n7d_split_builtin_sub_calling_custom_helper_anchored_regardless_of_interleaving() {
        // vcl_recv split into two (legal, builtin) fragments with an
        // unrelated acl interleaved between them; the first fragment calls
        // a plain (non-split) custom sub "helper".
        let decls_a = vec![
            sub("vcl_recv", vec![call("helper")]),
            acl("distract", vec![acl_entry("10.0.0.0", Some(8), false)]),
            sub("vcl_recv", vec![set(&["req", "http", "z"], str_("Z"))]),
            sub("helper", vec![set(&["req", "http", "a"], str_("A"))]),
        ];
        // Same fragments, but the interleaving acl now precedes both.
        let decls_b = vec![
            decls_a[1].clone(),
            decls_a[0].clone(),
            decls_a[2].clone(),
            decls_a[3].clone(),
        ];

        let mut pa = program(decls_a);
        let mut pb = program(decls_b);

        let map_a = run(&mut pa);
        let map_b = run(&mut pb);
        crate::normalize::sort::run(&mut pa);
        crate::normalize::sort::run(&mut pb);

        let ja = serde_json::to_value(&pa.decls).unwrap();
        let jb = serde_json::to_value(&pb.decls).unwrap();
        assert_eq!(ja, jb);

        // helper is renamed exactly once, anchored at the call inside the
        // first vcl_recv fragment, regardless of the acl's position.
        for map in [&map_a, &map_b] {
            let helper_entries: Vec<_> = map.entries.iter().filter(|e| e.2 == "helper").collect();
            assert_eq!(helper_entries.len(), 1, "helper renamed exactly once");
            assert_eq!(helper_entries[0].1, "sub_1");
        }

        // vcl_recv's merged body is [call helper, set z] in declaration order.
        for p in [&pa, &pb] {
            let recv = p
                .decls
                .iter()
                .find(|d| matches!(d, Decl::Sub{name, ..} if name == "vcl_recv"))
                .expect("vcl_recv present");
            if let Decl::Sub { body, .. } = recv {
                assert_eq!(body.len(), 2, "expected merged 2-statement vcl_recv body");
            } else {
                panic!("expected Decl::Sub");
            }
        }
    }

    #[test]
    fn n8_usage_order_anchoring() {
        // web_b is declared first but web_a is referenced first in vcl_recv.
        let mut p = program(vec![
            backend("web_b", vec![fexpr("host", str_("b.example.com"))]),
            backend("web_a", vec![fexpr("host", str_("a.example.com"))]),
            sub(
                "vcl_recv",
                vec![set(&["req", "http", "x"], var(&["web_a"]))],
            ),
        ]);

        let map = run(&mut p);

        let backend_name = |orig: &str| -> String {
            map.entries
                .iter()
                .find(|e| e.0 == "backend" && e.2 == orig)
                .unwrap_or_else(|| panic!("no rename entry for {orig}"))
                .1
                .clone()
        };
        assert_eq!(backend_name("web_a"), "backend_1");
        assert_eq!(backend_name("web_b"), "backend_2");
    }

    #[test]
    fn n9_call_dfs_diamond_visited_once() {
        // vcl_recv calls "top"; top calls "left" then "right"; both left and
        // right call "shared". Diamond call graph: shared is visited once,
        // numbered at the first call-site position (via left).
        let mut p = program(vec![
            sub("shared", vec![set(&["req", "http", "z"], str_("v"))]),
            sub("right", vec![call("shared")]),
            sub("left", vec![call("shared")]),
            sub("top", vec![call("left"), call("right")]),
            sub("vcl_recv", vec![call("top")]),
        ]);

        let map = run(&mut p);

        let sub_name = |orig: &str| -> String {
            map.entries
                .iter()
                .find(|e| e.0 == "sub" && e.2 == orig)
                .unwrap_or_else(|| panic!("no rename entry for {orig}"))
                .1
                .clone()
        };
        assert_eq!(sub_name("top"), "sub_1");
        assert_eq!(sub_name("left"), "sub_2");
        assert_eq!(sub_name("shared"), "sub_3");
        assert_eq!(sub_name("right"), "sub_4");

        // Order in the NameMap reflects assignment order too.
        let names: Vec<&str> = map
            .entries
            .iter()
            .filter(|e| e.0 == "sub")
            .map(|e| e.2.as_str())
            .collect();
        assert_eq!(names, vec!["top", "left", "shared", "right"]);
    }

    #[test]
    fn unknown_vcl_prefixed_hook_still_dfs_anchors_its_calls() {
        // vcl_vha_internal-style scenario: a vcl_*-prefixed sub NOT in
        // BUILTIN_SUB_ORDER (so real-VCC-unenumerable, treated as a hook
        // per is_builtin_sub) that itself `call`s a custom sub. Without
        // walking it, "helper" would incorrectly fall into the dead
        // (unreferenced) tie-break pool instead of being anchored by its
        // real call site.
        let mut p = program(vec![
            sub("helper", vec![set(&["req", "http", "x"], str_("1"))]),
            sub("vcl_vha_internal", vec![call("helper")]),
        ]);

        let map = run(&mut p);

        let helper_entry = map
            .entries
            .iter()
            .find(|e| e.0 == "sub" && e.2 == "helper")
            .expect("helper should be renamed via the DFS walk, not the dead pool");
        assert_eq!(helper_entry.1, "sub_1");

        // vcl_vha_internal itself keeps its name (it's a hook).
        assert!(p
            .decls
            .iter()
            .any(|d| matches!(d, Decl::Sub{name, ..} if name == "vcl_vha_internal")));
    }

    #[test]
    fn is_builtin_sub_is_a_prefix_check() {
        // is_builtin_sub is deliberately a `starts_with("vcl_")` check, not
        // exact membership in BUILTIN_SUB_ORDER. An earlier version made it
        // exact-match-only, to catch a typo like `vcl_devliver` for
        // `vcl_deliver` -- but that broke real Varnish Enterprise VCL,
        // which legitimately declares additional `vcl_*`-prefixed hook
        // names outside the classic 14-hook set (not enumerable by us --
        // see ast::is_builtin_sub's doc comment for the real-world case
        // that proved it: `vcl_vha_internal`, declared twice across two
        // included files, accepted by real varnishd exactly like a core
        // hook). Any `vcl_`-prefixed name is treated uniformly.
        assert!(ast::is_builtin_sub("vcl_deliver"));
        assert!(ast::is_builtin_sub("vcl_recv"));
        assert!(ast::is_builtin_sub("vcl_devliver")); // typo of vcl_deliver -- still treated as a hook
        assert!(ast::is_builtin_sub("vcl_vha_internal")); // real Enterprise hook, not in BUILTIN_SUB_ORDER
        assert!(!ast::is_builtin_sub("vc_typo"));
        assert!(!ast::is_builtin_sub("helper"));
    }

    #[test]
    fn n10_builtin_and_default_keep_names_dead_tie_break_deterministic() {
        let mut p1 = program(vec![
            backend_none("default"),
            acl("z_acl", vec![acl_entry("1.2.3.4", Some(32), false)]),
            acl("a_acl", vec![acl_entry("1.2.3.4", Some(32), false)]),
            sub("vcl_recv", vec![call("vcl_hit")]),
        ]);
        let mut p2 = program(p1.decls.clone());

        let map1 = run(&mut p1);
        let map2 = run(&mut p2);

        // vcl_* builtin subs and the `default` backend keep their names and are
        // NOT recorded in the bijection (only renamed objects are).
        assert!(map1
            .entries
            .iter()
            .all(|e| e.2 != "vcl_recv" && e.2 != "default"));
        assert_eq!(
            p1.decls
                .iter()
                .find(|d| d.name() == "default")
                .unwrap()
                .name(),
            "default"
        );
        assert!(p1
            .decls
            .iter()
            .any(|d| matches!(d, Decl::Sub { name, .. } if name == "vcl_recv")));

        // Two structurally-identical dead ACLs get distinct canonical names,
        // deterministically (same result on repeated runs of the same input).
        let acl_names_1: Vec<String> = map1
            .entries
            .iter()
            .filter(|e| e.0 == "acl")
            .map(|e| e.1.clone())
            .collect();
        let acl_names_2: Vec<String> = map2
            .entries
            .iter()
            .filter(|e| e.0 == "acl")
            .map(|e| e.1.clone())
            .collect();
        assert_eq!(acl_names_1.len(), 2);
        assert_eq!(acl_names_1, acl_names_2);
    }

    #[test]
    fn n11_bijection_recorded_correctly() {
        let mut p = program(vec![
            backend("web1", vec![fexpr("host", str_("1.example.com"))]),
            probe("hc", vec![fexpr("url", str_("/health"))]),
            sub("vcl_recv", vec![set(&["req", "http", "x"], str_("1"))]),
        ]);
        // Wire the probe onto the backend via a ProbeRef (as pass 3 would leave it).
        if let Decl::Backend {
            body: Some(fields), ..
        } = &mut p.decls[0]
        {
            fields.push(field("probe", FieldValue::ProbeRef("hc".to_string())));
        }
        if let Decl::Sub { body, .. } = &mut p.decls[2] {
            body.push(set(&["req", "http", "y"], var(&["web1"])));
        }

        let map = run(&mut p);

        // web1 -> backend_1, and its probe hc -> probe_1, cascaded at backend
        // assignment time (both appear in the map since both are user objects).
        assert!(map.entries.contains(&(
            "backend".to_string(),
            "backend_1".to_string(),
            "web1".to_string()
        )));
        assert!(map.entries.contains(&(
            "probe".to_string(),
            "probe_1".to_string(),
            "hc".to_string()
        )));
        // vcl_recv is a builtin sub — never present in the map.
        assert!(!map.entries.iter().any(|e| e.2 == "vcl_recv"));
    }
}
