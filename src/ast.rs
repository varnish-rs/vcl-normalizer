//! Frozen AST contract for vcl-normalizer. Every module codes against these types.
//!
//! Spans are excluded from serialization (`#[serde(skip)]`): canonical JSON —
//! and therefore equivalence comparison — is span-free by construction.
//! Do NOT derive or rely on `PartialEq` across whole trees for equivalence;
//! compare canonical JSON (see `canon.rs`). Hand-built test ASTs use dummy
//! spans, so intra-test `PartialEq` on subtrees is fine.

use serde::Serialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Span {
    pub file: u32, // index into SourceMap.files
    pub lo: u32,   // byte offset, inclusive
    pub hi: u32,   // byte offset, exclusive
}

impl Span {
    pub fn dummy() -> Self {
        Span {
            file: 0,
            lo: 0,
            hi: 0,
        }
    }
}

#[derive(Debug, Default)]
pub struct SourceMap {
    pub files: Vec<(PathBuf, String)>,
}

impl SourceMap {
    pub fn add(&mut self, path: PathBuf, text: String) -> u32 {
        self.files.push((path, text));
        (self.files.len() - 1) as u32
    }

    /// Resolve a span to (path, 1-based line, 1-based col).
    pub fn resolve(&self, span: Span) -> (&PathBuf, u32, u32) {
        let (path, text) = &self.files[span.file as usize];
        let upto = &text[..(span.lo as usize).min(text.len())];
        let line = upto.bytes().filter(|&b| b == b'\n').count() as u32 + 1;
        let col = (upto.len() - upto.rfind('\n').map(|i| i + 1).unwrap_or(0)) as u32 + 1;
        (path, line, col)
    }

    /// The source line containing the span start (for caret diagnostics).
    pub fn line_text(&self, span: Span) -> &str {
        let (_, text) = &self.files[span.file as usize];
        let lo = (span.lo as usize).min(text.len());
        let start = text[..lo].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let end = text[lo..].find('\n').map(|i| lo + i).unwrap_or(text.len());
        &text[start..end]
    }
}

/// Shared error type for lexer / parser / validation. Exit code 2.
#[derive(Debug)]
pub struct VclError {
    pub span: Option<Span>,
    pub msg: String,
}

impl VclError {
    pub fn new(span: Span, msg: impl Into<String>) -> Self {
        VclError {
            span: Some(span),
            msg: msg.into(),
        }
    }

    pub fn render(&self, sm: &SourceMap) -> String {
        match self.span {
            Some(span) => {
                let (path, line, col) = sm.resolve(span);
                let src = sm.line_text(span);
                let caret = " ".repeat((col as usize).saturating_sub(1)) + "^";
                format!(
                    "{}:{}:{}: {}\n  {}\n  {}",
                    path.display(),
                    line,
                    col,
                    self.msg,
                    src,
                    caret
                )
            }
            None => self.msg.clone(),
        }
    }
}

pub type Result<T> = std::result::Result<T, VclError>;

// ────────────────────── source comments (print-only) ──────────────────────
//
// Comments are trivia: never part of the canonical JSON `dump`/`compare`
// rely on for equivalence (see module doc above), only ever consulted by
// `printer.rs` for `print`. Attachment is keyed by the `Span` of whichever
// AST node (Decl/Stmt/Field/AclEntry/Arg) a comment attaches to, rather than
// living on the node itself -- that keeps every existing struct/enum
// literal in the crate unchanged and sidesteps `PartialEq`/`Serialize`
// entirely (a `HashMap` has neither, and doesn't need either here).

/// One verbatim source comment, captured for `print` to re-emit.
#[derive(Debug, Clone)]
pub struct LeadingComment {
    /// Verbatim text including delimiter(s) (`#`, `//`, or `/* ... */`,
    /// internal newlines intact for block comments).
    pub text: String,
    /// True only for a builtin-sub fragment's comment reattached across a
    /// `normalize::rename::merge_only` merge -- forces column-0 rendering
    /// regardless of the attached statement's indent level.
    pub unindented: bool,
}

/// Comments attached to one AST node.
#[derive(Debug, Clone, Default)]
pub struct NodeComments {
    /// Standalone comment lines immediately preceding the node (no blank
    /// line or code between them and the node).
    pub leading: Vec<LeadingComment>,
    /// A comment sharing the node's own last source line.
    pub trailing: Option<String>,
    /// Orphan comments between this node and the next sibling that doesn't
    /// exist (i.e. this was the last item in its list) -- e.g. right before
    /// a closing `}`.
    pub after: Vec<LeadingComment>,
}

/// Source-comment attachment for a whole `Program`, keyed by a node's
/// starting position (`Span::file` + `Span::lo` -- two sibling nodes can
/// never start at the same byte offset, so this is unique; `hi` is
/// deliberately not part of the key). That's what lets the parser key a
/// comment to an enclosing node -- e.g. a `backend x { // note` same-line
/// comment attaching to the `backend` decl itself -- using just the
/// position it started at, before the decl's closing `}` (and so its full
/// `Span`) is even known yet.
#[derive(Debug, Clone, Default)]
pub struct CommentMap {
    by_span: std::collections::HashMap<(u32, u32), NodeComments>,
}

impl CommentMap {
    fn key(span: Span) -> (u32, u32) {
        (span.file, span.lo)
    }

    pub fn get(&self, span: Span) -> Option<&NodeComments> {
        self.by_span.get(&Self::key(span))
    }

    pub fn entry(&mut self, span: Span) -> &mut NodeComments {
        self.by_span.entry(Self::key(span)).or_default()
    }

    /// Removes and returns a node's comments (used when a `Decl` disappears
    /// entirely, e.g. a builtin-sub fragment absorbed by
    /// `normalize::rename::merge_only`).
    pub fn take(&mut self, span: Span) -> Option<NodeComments> {
        self.by_span.remove(&Self::key(span))
    }
}

// ───────────────────────────── AST ─────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct Program {
    // Kept for future diagnostics (e.g. pointing at the version decl); not
    // read by any current pass.
    #[serde(skip)]
    #[allow(dead_code)]
    pub vcl_version: Span,
    pub decls: Vec<Decl>,
    /// Comment attachment for every Decl/Stmt/Field/AclEntry/Arg in the
    /// tree. Print-only; never serialized.
    #[serde(skip)]
    pub comments: CommentMap,
    /// Orphan comments after the very last top-level decl (end of file).
    /// Kept separate from a per-decl `.after` because `normalize::sort`
    /// reorders top-level decls -- end-of-file comments must always print
    /// at the true end, not wherever the originally-last decl ends up.
    #[serde(skip)]
    pub trailing_comments: Vec<LeadingComment>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Decl {
    Import {
        name: String,
        from: Option<String>,
        #[serde(skip)]
        span: Span,
    },
    Backend {
        name: String,
        /// `backend x none;` → none = true, body = None
        none: bool,
        body: Option<Vec<Field>>,
        #[serde(skip)]
        span: Span,
    },
    Probe {
        name: String,
        body: Vec<Field>,
        #[serde(skip)]
        span: Span,
    },
    Acl {
        name: String,
        entries: Vec<AclEntry>,
        #[serde(skip)]
        span: Span,
    },
    Sub {
        name: String,
        body: Vec<Stmt>,
        #[serde(skip)]
        span: Span,
    },
}

impl Decl {
    pub fn name(&self) -> &str {
        match self {
            Decl::Import { name, .. }
            | Decl::Backend { name, .. }
            | Decl::Probe { name, .. }
            | Decl::Acl { name, .. }
            | Decl::Sub { name, .. } => name,
        }
    }

    pub fn span(&self) -> Span {
        match self {
            Decl::Import { span, .. }
            | Decl::Backend { span, .. }
            | Decl::Probe { span, .. }
            | Decl::Acl { span, .. }
            | Decl::Sub { span, .. } => *span,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Field {
    pub name: String, // without the leading '.'
    pub value: FieldValue,
    #[serde(skip)]
    pub span: Span,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum FieldValue {
    Expr(Expr),
    /// Inline probe: `.probe = { .url = "/"; }` — lifted out by normalize pass 3.
    Probe(Vec<Field>),
    /// Probe reference: `.probe = my_probe;`
    ProbeRef(String),
    /// `.request = "GET / HTTP/1.1" "Host: x";` (2+ adjacent strings, CRLF-joined at runtime)
    StringList(Vec<String>),
}

#[derive(Debug, Clone, Serialize)]
pub struct AclEntry {
    pub negated: bool,
    pub addr: String,
    pub mask: Option<u8>,
    #[serde(skip)]
    pub span: Span,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Stmt {
    Set {
        lhs: Lvalue,
        rhs: Expr,
        #[serde(skip)]
        span: Span,
    },
    Unset {
        lhs: Lvalue,
        #[serde(skip)]
        span: Span,
    },
    Call {
        sub: String,
        #[serde(skip)]
        span: Span,
    },
    Return {
        /// None for a bare `return;` (custom subs).
        action: Option<ReturnAction>,
        #[serde(skip)]
        span: Span,
    },
    Synthetic {
        value: Expr,
        #[serde(skip)]
        span: Span,
    },
    /// arms[0] is the `if`; arms[1..] are elsif/elseif/`else if` (one node for all spellings).
    If {
        arms: Vec<(Expr, Vec<Stmt>)>,
        else_body: Option<Vec<Stmt>>,
        #[serde(skip)]
        span: Span,
    },
    /// `new name = vmod.ctor(args);` — only legal in vcl_init.
    New {
        name: String,
        vmod: String,
        ctor: String,
        args: Vec<Arg>,
        #[serde(skip)]
        span: Span,
    },
    /// Bare call statement: `std.log("x");`
    Expr {
        expr: Expr,
        #[serde(skip)]
        span: Span,
    },
}

impl Stmt {
    pub fn span(&self) -> Span {
        match self {
            Stmt::Set { span, .. }
            | Stmt::Unset { span, .. }
            | Stmt::Call { span, .. }
            | Stmt::Return { span, .. }
            | Stmt::Synthetic { span, .. }
            | Stmt::If { span, .. }
            | Stmt::New { span, .. }
            | Stmt::Expr { span, .. } => *span,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ReturnAction {
    pub name: String, // lookup | pass | pipe | synth | vcl | …
    pub args: Vec<Expr>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(transparent)]
pub struct Lvalue {
    pub parts: Vec<String>, // e.g. ["req", "http", "cookie"]
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum Expr {
    Str(String),
    /// Raw numeric text at parse time; canonicalized by normalize pass 1.
    Num(String),
    /// Seconds. Filled by pass 1; parser emits Duration(seconds) directly from token.
    Duration(f64),
    /// Bytes.
    Bytes(u64),
    Bool(bool),
    /// Canonical marker for an omitted-or-default optional vmod argument (pass 2 only).
    Omitted,
    /// Inline C block `C{ … }C`, raw text, compared byte-for-byte.
    CSource(String),
    /// Variable or bare symbol reference: req.url, beresp.ttl, my_acl.
    Var(Vec<String>),
    /// std.tolower(...), myobj.method(...), regsub(...).
    Call {
        target: Vec<String>,
        args: Vec<Arg>,
    },
    Unary {
        op: UnOp,
        expr: Box<Expr>,
    },
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct Arg {
    /// Named argument (`resolve=true`); None = positional.
    pub name: Option<String>,
    pub value: Expr,
    /// Needed only to key `CommentMap` lookups for arg-level comments;
    /// otherwise unused (not read by any normalize pass).
    #[serde(skip)]
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnOp {
    Not, // !
    Neg, // -
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BinOp {
    Eq,       // ==
    Ne,       // !=
    Match,    // ~
    NotMatch, // !~
    Lt,
    Le,
    Gt,
    Ge,
    And, // &&
    Or,  // ||
    Add,
    Sub,
    Mul,
    Div,
}

/// Rename bijection recorded by normalize pass 4, for `--names` output.
/// entries: (kind, canonical, original), in assignment order.
#[derive(Debug, Clone, Default, Serialize)]
pub struct NameMap {
    pub entries: Vec<(String, String, String)>,
}

/// Fixed walk order of builtin subs for normalize pass 4. The first 14 are
/// the classic VCL 4.1 hooks (`vcl(7)`, present in every Varnish build).
/// `vcl_connect` is Varnish-Enterprise-specific (a connection-level hook
/// firing before any request is parsed, e.g. for PROXY-protocol handling)
/// -- confirmed real via a genuine customer `vcl.show` dump using it
/// (`/usr/share/varnish-plus/vcl/otel.vcl`), even though this sandbox's own
/// (non-Enterprise) `varnishd` build doesn't implement it. Trust real
/// customer data over this sandbox's more limited build when they disagree.
pub const BUILTIN_SUB_ORDER: &[&str] = &[
    "vcl_init",
    "vcl_connect",
    "vcl_recv",
    "vcl_pipe",
    "vcl_pass",
    "vcl_hash",
    "vcl_purge",
    "vcl_hit",
    "vcl_miss",
    "vcl_deliver",
    "vcl_synth",
    "vcl_backend_fetch",
    "vcl_backend_response",
    "vcl_backend_error",
    "vcl_fini",
];

/// A `vcl_` prefix check, NOT exact membership in `BUILTIN_SUB_ORDER`.
///
/// This was briefly exact-match-only (to catch a typo like `vcl_devliver`
/// for `vcl_deliver` -- real VCC does reject that: "the names 'vcl_*' are
/// reserved for subroutines"). That turned out to be too strict: real
/// Varnish Enterprise VCL legitimately declares additional `vcl_*`-prefixed
/// hook names that aren't part of the classic 14-hook `vcl(7)` set and
/// aren't enumerable by us -- e.g. a genuine customer `vcl.show` dump
/// declares `sub vcl_vha_internal` (VHA6/Enterprise-clustering-specific)
/// *twice*, in two different included files, and real `varnishd -C` accepts
/// it exactly like a core hook (concatenated, not a duplicate-declaration
/// error). Which `vcl_*` names are valid is apparently baked into each
/// specific Varnish build/version, not derivable from the VCL text or from
/// vmod `.so` specs -- so we can't reliably enumerate it. Falling back to
/// the permissive prefix check means we may occasionally fail to catch a
/// genuine typo (accepting it as if it were a real hook), but we never
/// reject legitimate Enterprise VCL -- consistent with this tool's stated
/// philosophy of deferring full validity checking to `varnishd -C`.
/// `symbols::validate` still emits a non-fatal warning for a `vcl_`-prefixed
/// name outside `BUILTIN_SUB_ORDER`, for whoever's watching stderr.
pub fn is_builtin_sub(name: &str) -> bool {
    name.starts_with("vcl_")
}

/// Duration unit → seconds multiplier (VCC's values; y = 365.25 d).
/// Used by the parser to build `Expr::Duration(seconds)` from a Duration token.
pub fn duration_secs(num: f64, unit: &str) -> Option<f64> {
    let mult = match unit {
        "ms" => 0.001,
        "s" => 1.0,
        "m" => 60.0,
        "h" => 3600.0,
        "d" => 86400.0,
        "w" => 604800.0,
        "y" => 31_557_600.0,
        _ => return None,
    };
    Some(num * mult)
}

/// Bytes unit → bytes (binary: KB = 1024). Used by the parser for Bytes tokens.
pub fn bytes_val(num: f64, unit: &str) -> Option<u64> {
    let mult: u64 = match unit {
        "B" => 1,
        "KB" => 1 << 10,
        "MB" => 1 << 20,
        "GB" => 1 << 30,
        "TB" => 1 << 40,
        _ => return None,
    };
    Some((num * mult as f64) as u64)
}

// ───────────────────────── test builder ─────────────────────────
// Terse constructors for hand-built ASTs (all dummy spans). Used by the
// normalize/printer/canon/compare unit tests so they don't need the parser.

#[cfg(any(test, feature = "testutil"))]
// Grab-bag of terse test-only constructors; not every one is necessarily
// exercised by the current test suite.
#[allow(dead_code)]
pub mod builder {
    use super::*;

    pub fn sp() -> Span {
        Span::dummy()
    }

    pub fn program(decls: Vec<Decl>) -> Program {
        Program {
            vcl_version: sp(),
            decls,
            comments: CommentMap::default(),
            trailing_comments: Vec::new(),
        }
    }

    pub fn import(name: &str) -> Decl {
        Decl::Import {
            name: name.into(),
            from: None,
            span: sp(),
        }
    }

    pub fn backend(name: &str, fields: Vec<Field>) -> Decl {
        Decl::Backend {
            name: name.into(),
            none: false,
            body: Some(fields),
            span: sp(),
        }
    }

    pub fn backend_none(name: &str) -> Decl {
        Decl::Backend {
            name: name.into(),
            none: true,
            body: None,
            span: sp(),
        }
    }

    pub fn probe(name: &str, fields: Vec<Field>) -> Decl {
        Decl::Probe {
            name: name.into(),
            body: fields,
            span: sp(),
        }
    }

    pub fn acl(name: &str, entries: Vec<AclEntry>) -> Decl {
        Decl::Acl {
            name: name.into(),
            entries,
            span: sp(),
        }
    }

    pub fn acl_entry(addr: &str, mask: Option<u8>, negated: bool) -> AclEntry {
        AclEntry {
            negated,
            addr: addr.into(),
            mask,
            span: sp(),
        }
    }

    pub fn sub(name: &str, body: Vec<Stmt>) -> Decl {
        Decl::Sub {
            name: name.into(),
            body,
            span: sp(),
        }
    }

    pub fn field(name: &str, value: FieldValue) -> Field {
        Field {
            name: name.into(),
            value,
            span: sp(),
        }
    }

    pub fn fexpr(name: &str, e: Expr) -> Field {
        field(name, FieldValue::Expr(e))
    }

    pub fn set(parts: &[&str], rhs: Expr) -> Stmt {
        Stmt::Set {
            lhs: lval(parts),
            rhs,
            span: sp(),
        }
    }

    pub fn unset(parts: &[&str]) -> Stmt {
        Stmt::Unset {
            lhs: lval(parts),
            span: sp(),
        }
    }

    pub fn call(sub: &str) -> Stmt {
        Stmt::Call {
            sub: sub.into(),
            span: sp(),
        }
    }

    pub fn ret(action: Option<ReturnAction>) -> Stmt {
        Stmt::Return { action, span: sp() }
    }

    pub fn ret_action(name: &str, args: Vec<Expr>) -> Stmt {
        ret(Some(ReturnAction {
            name: name.into(),
            args,
        }))
    }

    pub fn synthetic(value: Expr) -> Stmt {
        Stmt::Synthetic { value, span: sp() }
    }

    pub fn if_(arms: Vec<(Expr, Vec<Stmt>)>, else_body: Option<Vec<Stmt>>) -> Stmt {
        Stmt::If {
            arms,
            else_body,
            span: sp(),
        }
    }

    pub fn new_(name: &str, vmod: &str, ctor: &str, args: Vec<Arg>) -> Stmt {
        Stmt::New {
            name: name.into(),
            vmod: vmod.into(),
            ctor: ctor.into(),
            args,
            span: sp(),
        }
    }

    pub fn expr_stmt(expr: Expr) -> Stmt {
        Stmt::Expr { expr, span: sp() }
    }

    pub fn lval(parts: &[&str]) -> Lvalue {
        Lvalue {
            parts: parts.iter().map(|s| s.to_string()).collect(),
        }
    }

    pub fn str_(s: &str) -> Expr {
        Expr::Str(s.into())
    }

    pub fn num(s: &str) -> Expr {
        Expr::Num(s.into())
    }

    pub fn dur(secs: f64) -> Expr {
        Expr::Duration(secs)
    }

    pub fn bytes(n: u64) -> Expr {
        Expr::Bytes(n)
    }

    pub fn bool_(b: bool) -> Expr {
        Expr::Bool(b)
    }

    pub fn var(parts: &[&str]) -> Expr {
        Expr::Var(parts.iter().map(|s| s.to_string()).collect())
    }

    pub fn fcall(target: &[&str], args: Vec<Arg>) -> Expr {
        Expr::Call {
            target: target.iter().map(|s| s.to_string()).collect(),
            args,
        }
    }

    pub fn arg(value: Expr) -> Arg {
        Arg {
            name: None,
            value,
            span: sp(),
        }
    }

    pub fn narg(name: &str, value: Expr) -> Arg {
        Arg {
            name: Some(name.into()),
            value,
            span: sp(),
        }
    }

    pub fn un(op: UnOp, e: Expr) -> Expr {
        Expr::Unary {
            op,
            expr: Box::new(e),
        }
    }

    pub fn bin(op: BinOp, l: Expr, r: Expr) -> Expr {
        Expr::Binary {
            op,
            lhs: Box::new(l),
            rhs: Box::new(r),
        }
    }
}
