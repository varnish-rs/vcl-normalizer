//! Hand-written recursive-descent parser over the [`crate::lexer`] token
//! stream, producing the frozen [`crate::ast`] types. See spec §4 for the
//! full EBNF.

use crate::ast::{
    self, AclEntry, Arg, BinOp, Decl, Expr, Field, FieldValue, Lvalue, Program, Result,
    ReturnAction, Span, Stmt, UnOp, VclError,
};
use crate::lexer::{self, Tok, Token};
use std::path::{Path, PathBuf};

/// Parse a root VCL file (splicing includes), returning the AST plus the
/// `SourceMap` needed to resolve spans for diagnostics/reports.
pub fn parse_file(path: &Path, include_dirs: &[PathBuf]) -> Result<(Program, ast::SourceMap)> {
    let (tokens, comments, sm) = lexer::lex(path, include_dirs)?;
    let mut p = Parser::new(&tokens, &comments);
    let prog = p.parse_program()?;
    Ok((prog, sm))
}

/// Convenience for tests: parse a string with a dummy path and no includes.
#[cfg(test)]
pub fn parse_str(src: &str) -> Result<Program> {
    let (tokens, comments, _sm) = lexer::lex_str(src)?;
    let mut p = Parser::new(&tokens, &comments);
    p.parse_program()
}

/// Parses a `vcl.show`-dump file (see `crate::vclshow`): resolves every
/// `include` against the dump's own chunks, in document order, never
/// touching the filesystem.
pub fn parse_vcl_show_file(path: &Path) -> Result<(Program, ast::SourceMap)> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        VclError::new(
            Span::dummy(),
            format!("cannot read '{}': {e}", path.display()),
        )
    })?;
    let chunks = crate::vclshow::parse_chunks(&text)?;
    let (tokens, comments, sm) = crate::lexer::lex_from_vcl_show(&chunks)?;
    let mut p = Parser::new(&tokens, &comments);
    let prog = p.parse_program()?;
    Ok((prog, sm))
}

struct Parser<'a> {
    toks: &'a [Token],
    pos: usize,
    /// Comments in source order (see `lexer::RawComment`), consumed
    /// strictly left-to-right in lockstep with the token stream -- `cidx`
    /// only ever advances, and every list's `gap` calls (see below) happen
    /// incrementally, right before parsing each item, rather than
    /// batched after the fact. That ordering matters: it's what lets an
    /// enclosing list settle "everything up to my next item" (including a
    /// same-line trailing comment on itself) *before* control passes down
    /// into that item's own nested lists -- otherwise a nested list would
    /// unavoidably race the enclosing one for comments that chronologically
    /// come first but semantically belong one level up.
    comments: &'a [lexer::RawComment],
    cidx: usize,
    comments_out: ast::CommentMap,
}

fn span_from(start: Span, end: Span) -> Span {
    Span {
        file: start.file,
        lo: start.lo,
        hi: end.hi,
    }
}

impl<'a> Parser<'a> {
    fn new(toks: &'a [Token], comments: &'a [lexer::RawComment]) -> Self {
        Parser {
            toks,
            pos: 0,
            comments,
            cidx: 0,
            comments_out: ast::CommentMap::default(),
        }
    }

    /// Drains (and returns) all not-yet-consumed comments strictly before
    /// byte offset `before`, advancing the shared cursor past them.
    fn drain_comments_before(&mut self, before: u32) -> &'a [lexer::RawComment] {
        let comments = self.comments;
        let start = self.cidx;
        let mut end = start;
        while end < comments.len() && comments[end].span.lo < before {
            end += 1;
        }
        self.cidx = end;
        &comments[start..end]
    }

    /// Routes one gap's drained comments: an initial same-line-as-`prev`
    /// run becomes `prev`'s single inline `trailing` comment (only
    /// meaningful if `prev` exists -- the gap before the very first item
    /// in a list has no earlier sibling to trail); everything after that
    /// is a standalone leading-comment block. If `next` exists, that block
    /// becomes `next`'s `leading`; otherwise it's returned to the caller,
    /// which decides where trailing/orphan comments at the end of a list
    /// belong (usually the last item's `after`, but top-level decls route
    /// to `Program.trailing_comments` instead -- see `parse_program`).
    fn route_gap(
        &mut self,
        drained: &[lexer::RawComment],
        prev: Option<Span>,
        next: Option<Span>,
    ) -> Vec<ast::LeadingComment> {
        let mut i = 0;
        if let Some(prev_sp) = prev {
            if i < drained.len() && !drained[i].standalone {
                let mut text = drained[i].text.clone();
                i += 1;
                while i < drained.len() && !drained[i].standalone {
                    text.push('\n');
                    text.push_str(&drained[i].text);
                    i += 1;
                }
                self.comments_out.entry(prev_sp).trailing = Some(text);
            }
        }
        let leading: Vec<ast::LeadingComment> = drained[i..]
            .iter()
            .map(|c| ast::LeadingComment {
                text: c.text.clone(),
                unindented: false,
            })
            .collect();
        match next {
            Some(next_sp) => {
                if !leading.is_empty() {
                    self.comments_out.entry(next_sp).leading = leading;
                }
                Vec::new()
            }
            None => leading,
        }
    }

    /// Drains and routes exactly one gap. Must be called in true document
    /// order, immediately before parsing whatever `next` (if `Some`) refers
    /// to -- one call per gap, working strictly left to right. That's what
    /// stops a nested list from stealing a comment meant for an enclosing
    /// one: e.g. for `backend x { // note` + fields, the decl-level caller
    /// asks about "everything up through `{`" (settling `x`'s own
    /// `trailing`) *before* the field parser ever starts asking about gaps
    /// between fields -- so by the time field parsing begins, that
    /// same-line comment is already spoken for. `next = None` means this is
    /// a list's final gap (nothing follows) -- the returned overflow is for
    /// the caller to place (usually the last item's `after`, but top-level
    /// decls route to `Program.trailing_comments` instead, since
    /// `normalize::sort` can reorder them -- see `parse_program`).
    fn gap(
        &mut self,
        prev: Option<Span>,
        before: u32,
        next: Option<Span>,
    ) -> Vec<ast::LeadingComment> {
        let drained = self.drain_comments_before(before).to_vec();
        self.route_gap(&drained, prev, next)
    }

    /// Closes out a list: routes the final gap (nothing follows) and
    /// attaches any overflow as `after` on whichever node is left in
    /// `prev` -- the last real item if the list wasn't empty, or still the
    /// list's `enclosing` node (see `gap`'s doc comment) if it was.
    fn finish_gap(&mut self, prev: Option<Span>, container_hi: u32) {
        let overflow = self.gap(prev, container_hi, None);
        if let (Some(target), false) = (prev, overflow.is_empty()) {
            self.comments_out.entry(target).after = overflow;
        }
    }

    fn cur(&self) -> &Token {
        &self.toks[self.pos]
    }

    fn peek(&self) -> &Tok {
        &self.cur().tok
    }

    fn peek_span(&self) -> Span {
        self.cur().span
    }

    fn peek_at(&self, k: usize) -> &Tok {
        let idx = (self.pos + k).min(self.toks.len() - 1);
        &self.toks[idx].tok
    }

    fn prev_span(&self) -> Span {
        let idx = self.pos.saturating_sub(1);
        self.toks[idx].span
    }

    fn advance(&mut self) -> Token {
        let t = self.cur().clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }

    fn is_ident(&self, s: &str) -> bool {
        matches!(self.peek(), Tok::Ident(x) if x == s)
    }

    fn error(&self, msg: impl Into<String>) -> VclError {
        VclError::new(self.peek_span(), msg.into())
    }

    fn expect(&mut self, want: Tok) -> Result<Token> {
        if self.peek() == &want {
            Ok(self.advance())
        } else {
            Err(self.error(format!("expected {:?}, found {:?}", want, self.peek())))
        }
    }

    fn expect_ident_kw(&mut self, kw: &str) -> Result<Token> {
        if self.is_ident(kw) {
            Ok(self.advance())
        } else {
            Err(self.error(format!("expected '{kw}', found {:?}", self.peek())))
        }
    }

    fn expect_any_ident(&mut self) -> Result<(String, Span)> {
        match self.peek().clone() {
            Tok::Ident(s) => {
                let sp = self.peek_span();
                self.advance();
                Ok((s, sp))
            }
            other => Err(self.error(format!("expected identifier, found {other:?}"))),
        }
    }

    fn expect_str(&mut self) -> Result<(String, Span)> {
        match self.peek().clone() {
            Tok::Str(s) => {
                let sp = self.peek_span();
                self.advance();
                Ok((s, sp))
            }
            other => Err(self.error(format!("expected string literal, found {other:?}"))),
        }
    }

    // ───────────────────────── top-level ─────────────────────────

    fn parse_program(&mut self) -> Result<Program> {
        let vcl_version = self.parse_vcl_decl()?;
        let mut decls: Vec<Decl> = Vec::new();
        let mut prev: Option<Span> = None;
        loop {
            if matches!(self.peek(), Tok::Eof) {
                break;
            }
            // Handle secondary vcl declarations (tolerated as no-ops, even on version mismatch)
            if self.is_ident("vcl") {
                self.parse_secondary_vcl_decl()?;
                continue;
            }
            let next = self.peek_span();
            self.gap(prev, next.lo, Some(next));
            let d = self.parse_top_decl()?;
            prev = Some(d.span());
            decls.push(d);
        }
        let eof = self.peek_span();
        // Final-gap overflow goes to `trailing_comments`, not the last
        // decl's `after` -- `normalize::sort` can reorder top-level decls,
        // so end-of-file comments must always print at the true end
        // rather than wherever the originally-last decl ends up.
        let trailing_comments = self.gap(prev, eof.lo, None);
        Ok(Program {
            vcl_version,
            decls,
            comments: std::mem::take(&mut self.comments_out),
            trailing_comments,
        })
    }

    /// Parses version numbers (major.minor) from either a single "X.Y" token or
    /// two separate "X . Y" tokens. Returns the major and minor version strings
    /// plus the span covering the numeric tokens.
    fn parse_version_numbers(&mut self) -> Result<(String, String, Span)> {
        match self.peek().clone() {
            Tok::Num(n) if n.contains('.') => {
                let sp = self.peek_span();
                self.advance();
                let mut parts = n.splitn(2, '.');
                let major = parts.next().unwrap_or_default().to_string();
                let minor = parts.next().unwrap_or_default().to_string();
                Ok((major, minor, sp))
            }
            Tok::Num(n) => {
                let major = n.clone();
                let start_span = self.peek_span();
                self.advance();
                self.expect(Tok::Dot)?;
                let (minor, minor_sp) = match self.peek().clone() {
                    Tok::Num(m) => {
                        let sp = self.peek_span();
                        self.advance();
                        (m, sp)
                    }
                    other => {
                        return Err(
                            self.error(format!("expected version minor number, found {other:?}"))
                        )
                    }
                };
                Ok((major, minor, span_from(start_span, minor_sp)))
            }
            other => Err(self.error(format!("expected VCL version number, found {other:?}"))),
        }
    }

    fn parse_vcl_decl(&mut self) -> Result<Span> {
        let start = self.peek_span();
        self.expect_ident_kw("vcl")?;
        let (major, minor, ver_span) = self.parse_version_numbers()?;
        if major != "4" || minor != "1" {
            return Err(VclError::new(ver_span, "only VCL 4.1 supported"));
        }
        let semi = self.expect(Tok::Semi)?;
        Ok(span_from(start, semi.span))
    }

    /// Parses a secondary `vcl X.Y;` declaration (one that appears after the
    /// initial mandatory one). These are tolerated as no-ops, even on version
    /// mismatch, mirroring real Varnish behavior.
    fn parse_secondary_vcl_decl(&mut self) -> Result<()> {
        self.expect_ident_kw("vcl")?;
        let (_major, _minor, _ver_span) = self.parse_version_numbers()?;
        self.expect(Tok::Semi)?;
        Ok(())
    }

    fn parse_top_decl(&mut self) -> Result<Decl> {
        match self.peek().clone() {
            Tok::Ident(kw) if kw == "import" => self.parse_import(),
            Tok::Ident(kw) if kw == "backend" => self.parse_backend(),
            Tok::Ident(kw) if kw == "probe" => self.parse_probe(),
            Tok::Ident(kw) if kw == "acl" => self.parse_acl(),
            Tok::Ident(kw) if kw == "sub" => self.parse_sub(),
            other => Err(self.error(format!(
                "expected top-level declaration (import/backend/probe/acl/sub), found {other:?}"
            ))),
        }
    }

    fn parse_import(&mut self) -> Result<Decl> {
        let start = self.peek_span();
        self.advance(); // 'import'
        let (name, _) = self.expect_any_ident()?;
        let mut from = None;
        if self.is_ident("from") {
            self.advance();
            let (s, _) = self.expect_str()?;
            from = Some(s);
        }
        let semi = self.expect(Tok::Semi)?;
        Ok(Decl::Import {
            name,
            from,
            span: span_from(start, semi.span),
        })
    }

    fn parse_backend(&mut self) -> Result<Decl> {
        let start = self.peek_span();
        self.advance(); // 'backend'
        let (name, _) = self.expect_any_ident()?;
        if self.is_ident("none") {
            self.advance();
            let semi = self.expect(Tok::Semi)?;
            return Ok(Decl::Backend {
                name,
                none: true,
                body: None,
                span: span_from(start, semi.span),
            });
        }
        self.expect(Tok::LBrace)?;
        let mut fields = Vec::new();
        let mut prev = Some(start); // enclosing: same-line comment after '{' trails the decl itself
        while !matches!(self.peek(), Tok::RBrace) {
            let next = self.peek_span();
            self.gap(prev, next.lo, Some(next));
            let f = self.parse_field()?;
            prev = Some(f.span);
            fields.push(f);
        }
        let rbrace = self.expect(Tok::RBrace)?;
        self.finish_gap(prev, rbrace.span.lo);
        Ok(Decl::Backend {
            name,
            none: false,
            body: Some(fields),
            span: span_from(start, rbrace.span),
        })
    }

    fn parse_probe(&mut self) -> Result<Decl> {
        let start = self.peek_span();
        self.advance(); // 'probe'
        let (name, _) = self.expect_any_ident()?;
        self.expect(Tok::LBrace)?;
        let mut fields = Vec::new();
        let mut prev = Some(start);
        while !matches!(self.peek(), Tok::RBrace) {
            let next = self.peek_span();
            self.gap(prev, next.lo, Some(next));
            let f = self.parse_field()?;
            prev = Some(f.span);
            fields.push(f);
        }
        let rbrace = self.expect(Tok::RBrace)?;
        self.finish_gap(prev, rbrace.span.lo);
        Ok(Decl::Probe {
            name,
            body: fields,
            span: span_from(start, rbrace.span),
        })
    }

    fn parse_acl(&mut self) -> Result<Decl> {
        let start = self.peek_span();
        self.advance(); // 'acl'
        let (name, _) = self.expect_any_ident()?;
        self.expect(Tok::LBrace)?;
        let mut entries = Vec::new();
        let mut prev = Some(start);
        while !matches!(self.peek(), Tok::RBrace) {
            let next = self.peek_span();
            self.gap(prev, next.lo, Some(next));
            let e = self.parse_acl_entry()?;
            prev = Some(e.span);
            entries.push(e);
        }
        let rbrace = self.expect(Tok::RBrace)?;
        self.finish_gap(prev, rbrace.span.lo);
        Ok(Decl::Acl {
            name,
            entries,
            span: span_from(start, rbrace.span),
        })
    }

    fn parse_acl_entry(&mut self) -> Result<AclEntry> {
        let start = self.peek_span();
        let negated = if matches!(self.peek(), Tok::Bang) {
            self.advance();
            true
        } else {
            false
        };
        let (addr, _) = self.expect_str()?;
        let mut mask = None;
        if matches!(self.peek(), Tok::Slash) {
            self.advance();
            match self.peek().clone() {
                Tok::Num(n) => {
                    self.advance();
                    let v: u8 = n.parse().map_err(|_| self.error("invalid ACL mask"))?;
                    mask = Some(v);
                }
                other => return Err(self.error(format!("expected mask number, found {other:?}"))),
            }
        }
        let semi = self.expect(Tok::Semi)?;
        Ok(AclEntry {
            negated,
            addr,
            mask,
            span: span_from(start, semi.span),
        })
    }

    fn parse_sub(&mut self) -> Result<Decl> {
        let start = self.peek_span();
        self.advance(); // 'sub'
        let (name, _) = self.expect_any_ident()?;
        let body = self.parse_block(Some(start))?;
        let end = self.prev_span();
        Ok(Decl::Sub {
            name,
            body,
            span: span_from(start, end),
        })
    }

    fn parse_field(&mut self) -> Result<Field> {
        let start = self.peek_span();
        self.expect(Tok::Dot)?;
        let (name, _) = self.expect_any_ident()?;
        self.expect(Tok::Eq)?;
        let value = self.parse_field_value(start)?;
        // An inline probe block's closing '}' is the terminator -- real VCC
        // does not allow (and rejects) a trailing ';' after it, unlike every
        // other field-value form. Confirmed against `varnishd -C`.
        let end = if matches!(value, FieldValue::Probe(_)) {
            self.prev_span()
        } else {
            self.expect(Tok::Semi)?.span
        };
        Ok(Field {
            name,
            value,
            span: span_from(start, end),
        })
    }

    /// `field_start` is the enclosing `.name = ` field's own start (its
    /// eventual `Field.span.lo`) -- used only for a same-line comment right
    /// after an inline probe's opening `{` to trail the field itself,
    /// rather than being misattached as the first nested field's leading.
    fn parse_field_value(&mut self, field_start: Span) -> Result<FieldValue> {
        match self.peek().clone() {
            Tok::LBrace => {
                self.expect(Tok::LBrace)?;
                let mut fields = Vec::new();
                let mut prev = Some(field_start);
                while !matches!(self.peek(), Tok::RBrace) {
                    let next = self.peek_span();
                    self.gap(prev, next.lo, Some(next));
                    let f = self.parse_field()?;
                    prev = Some(f.span);
                    fields.push(f);
                }
                let rbrace = self.expect(Tok::RBrace)?;
                self.finish_gap(prev, rbrace.span.lo);
                Ok(FieldValue::Probe(fields))
            }
            // Bare identifier immediately followed by ';' (no dot, no call
            // parens) is a probe reference — unless it's the true/false
            // literal, which is a normal boolean expr.
            Tok::Ident(name)
                if name != "true" && name != "false" && matches!(self.peek_at(1), Tok::Semi) =>
            {
                self.advance();
                Ok(FieldValue::ProbeRef(name))
            }
            // 2+ adjacent string literals -> string list.
            Tok::Str(_) if matches!(self.peek_at(1), Tok::Str(_)) => {
                let mut strs = Vec::new();
                while let Tok::Str(s) = self.peek().clone() {
                    strs.push(s);
                    self.advance();
                }
                Ok(FieldValue::StringList(strs))
            }
            _ => Ok(FieldValue::Expr(self.parse_expr()?)),
        }
    }

    // ───────────────────────── statements ─────────────────────────

    /// `enclosing`, when given, is the span (well, its `.lo` -- see `gap`'s
    /// doc comment) a same-line comment right after this block's opening
    /// `{` should trail -- e.g. the owning `sub name {` line. `None` for
    /// an if-arm/else body: those share one `Stmt::If` span across
    /// multiple arms, so there's no single node to trail there; a
    /// same-line comment right after such an arm's own `{` falls back to
    /// being the first stmt's leading instead (a narrow, documented gap).
    fn parse_block(&mut self, enclosing: Option<Span>) -> Result<Vec<Stmt>> {
        self.expect(Tok::LBrace)?;
        let mut stmts = Vec::new();
        let mut prev = enclosing;
        loop {
            match self.peek() {
                Tok::RBrace => break,
                Tok::Eof => return Err(self.error("unterminated block, expected '}'")),
                _ => {
                    let next = self.peek_span();
                    self.gap(prev, next.lo, Some(next));
                    let s = self.parse_stmt()?;
                    prev = Some(s.span());
                    stmts.push(s);
                }
            }
        }
        let rbrace = self.expect(Tok::RBrace)?;
        self.finish_gap(prev, rbrace.span.lo);
        Ok(stmts)
    }

    fn parse_stmt(&mut self) -> Result<Stmt> {
        let start = self.peek_span();
        match self.peek().clone() {
            Tok::CSource(s) => {
                self.advance();
                Ok(Stmt::Expr {
                    expr: Expr::CSource(s),
                    span: start,
                })
            }
            Tok::Ident(kw) if kw == "set" => self.parse_set(start),
            Tok::Ident(kw) if kw == "unset" => self.parse_unset(start),
            Tok::Ident(kw) if kw == "call" => self.parse_call(start),
            Tok::Ident(kw) if kw == "return" => self.parse_return(start),
            Tok::Ident(kw) if kw == "synthetic" => self.parse_synthetic(start),
            Tok::Ident(kw) if kw == "new" => self.parse_new(start),
            Tok::Ident(kw) if kw == "if" => self.parse_if(start),
            Tok::Ident(_) => self.parse_expr_stmt(start),
            other => Err(self.error(format!("expected statement, found {other:?}"))),
        }
    }

    fn parse_set(&mut self, start: Span) -> Result<Stmt> {
        self.advance(); // 'set'
        let (parts, _) = self.parse_dotted_ident()?;
        self.expect(Tok::Eq)?;
        let rhs = self.parse_expr()?;
        let semi = self.expect(Tok::Semi)?;
        Ok(Stmt::Set {
            lhs: Lvalue { parts },
            rhs,
            span: span_from(start, semi.span),
        })
    }

    fn parse_unset(&mut self, start: Span) -> Result<Stmt> {
        self.advance(); // 'unset'
        let (parts, _) = self.parse_dotted_ident()?;
        let semi = self.expect(Tok::Semi)?;
        Ok(Stmt::Unset {
            lhs: Lvalue { parts },
            span: span_from(start, semi.span),
        })
    }

    fn parse_call(&mut self, start: Span) -> Result<Stmt> {
        self.advance(); // 'call'
        let (sub, _) = self.expect_any_ident()?;
        let semi = self.expect(Tok::Semi)?;
        Ok(Stmt::Call {
            sub,
            span: span_from(start, semi.span),
        })
    }

    fn parse_return(&mut self, start: Span) -> Result<Stmt> {
        self.advance(); // 'return'
        let mut action = None;
        if matches!(self.peek(), Tok::LParen) {
            self.advance();
            let (name, _) = self.expect_any_ident()?;
            let mut args = Vec::new();
            if matches!(self.peek(), Tok::LParen) {
                self.advance();
                if !matches!(self.peek(), Tok::RParen) {
                    args = self.parse_expr_list()?;
                }
                self.expect(Tok::RParen)?;
            }
            self.expect(Tok::RParen)?;
            action = Some(ReturnAction { name, args });
        }
        let semi = self.expect(Tok::Semi)?;
        Ok(Stmt::Return {
            action,
            span: span_from(start, semi.span),
        })
    }

    fn parse_synthetic(&mut self, start: Span) -> Result<Stmt> {
        self.advance(); // 'synthetic'
        self.expect(Tok::LParen)?;
        let value = self.parse_expr()?;
        self.expect(Tok::RParen)?;
        let semi = self.expect(Tok::Semi)?;
        Ok(Stmt::Synthetic {
            value,
            span: span_from(start, semi.span),
        })
    }

    fn parse_new(&mut self, start: Span) -> Result<Stmt> {
        self.advance(); // 'new'
        let (name, _) = self.expect_any_ident()?;
        self.expect(Tok::Eq)?;
        let (vmod, _) = self.expect_any_ident()?;
        self.expect(Tok::Dot)?;
        let (ctor, _) = self.expect_any_ident()?;
        self.expect(Tok::LParen)?;
        let args = if matches!(self.peek(), Tok::RParen) {
            Vec::new()
        } else {
            self.parse_arg_list()?
        };
        self.expect(Tok::RParen)?;
        let semi = self.expect(Tok::Semi)?;
        Ok(Stmt::New {
            name,
            vmod,
            ctor,
            args,
            span: span_from(start, semi.span),
        })
    }

    fn parse_expr_stmt(&mut self, start: Span) -> Result<Stmt> {
        let (parts, _) = self.parse_dotted_ident()?;
        self.expect(Tok::LParen)?;
        let args = if matches!(self.peek(), Tok::RParen) {
            Vec::new()
        } else {
            self.parse_arg_list()?
        };
        self.expect(Tok::RParen)?;
        let semi = self.expect(Tok::Semi)?;
        Ok(Stmt::Expr {
            expr: Expr::Call {
                target: parts,
                args,
            },
            span: span_from(start, semi.span),
        })
    }

    fn parse_if(&mut self, start: Span) -> Result<Stmt> {
        self.advance(); // 'if'
        self.expect(Tok::LParen)?;
        let cond = self.parse_expr()?;
        self.expect(Tok::RParen)?;
        let body = self.parse_block(None)?;
        let mut arms = vec![(cond, body)];
        let mut else_body = None;
        loop {
            if self.is_ident("elsif") || self.is_ident("elseif") {
                self.advance();
                self.expect(Tok::LParen)?;
                let c = self.parse_expr()?;
                self.expect(Tok::RParen)?;
                let b = self.parse_block(None)?;
                arms.push((c, b));
            } else if self.is_ident("else") && matches!(self.peek_at(1), Tok::Ident(s) if s == "if")
            {
                self.advance(); // 'else'
                self.advance(); // 'if'
                self.expect(Tok::LParen)?;
                let c = self.parse_expr()?;
                self.expect(Tok::RParen)?;
                let b = self.parse_block(None)?;
                arms.push((c, b));
            } else if self.is_ident("else") {
                self.advance();
                let b = self.parse_block(None)?;
                else_body = Some(b);
                break;
            } else {
                break;
            }
        }
        let end = self.prev_span();
        Ok(Stmt::If {
            arms,
            else_body,
            span: span_from(start, end),
        })
    }

    // ───────────────────────── expressions ─────────────────────────

    fn parse_dotted_ident(&mut self) -> Result<(Vec<String>, Span)> {
        let (first, first_span) = self.expect_any_ident()?;
        let mut parts = vec![first];
        let mut end = first_span;
        while matches!(self.peek(), Tok::Dot) {
            self.advance();
            let (id, sp) = self.expect_any_ident()?;
            parts.push(id);
            end = sp;
        }
        Ok((parts, span_from(first_span, end)))
    }

    fn parse_expr(&mut self) -> Result<Expr> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_and()?;
        while matches!(self.peek(), Tok::OrOr) {
            self.advance();
            let rhs = self.parse_and()?;
            lhs = Expr::Binary {
                op: BinOp::Or,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_cmp()?;
        while matches!(self.peek(), Tok::AndAnd) {
            self.advance();
            let rhs = self.parse_cmp()?;
            lhs = Expr::Binary {
                op: BinOp::And,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn cmp_op(&self) -> Option<BinOp> {
        match self.peek() {
            Tok::EqEq => Some(BinOp::Eq),
            Tok::Neq => Some(BinOp::Ne),
            Tok::Tilde => Some(BinOp::Match),
            Tok::NTilde => Some(BinOp::NotMatch),
            Tok::Lt => Some(BinOp::Lt),
            Tok::Leq => Some(BinOp::Le),
            Tok::Gt => Some(BinOp::Gt),
            Tok::Geq => Some(BinOp::Ge),
            _ => None,
        }
    }

    /// Non-associative: at most one comparison operator.
    fn parse_cmp(&mut self) -> Result<Expr> {
        let lhs = self.parse_add()?;
        match self.cmp_op() {
            None => Ok(lhs),
            Some(op) => {
                self.advance();
                let rhs = self.parse_add()?;
                if self.cmp_op().is_some() {
                    return Err(self.error("comparison operators are non-associative"));
                }
                Ok(Expr::Binary {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                })
            }
        }
    }

    fn parse_add(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_mul()?;
        loop {
            let op = match self.peek() {
                Tok::Plus => BinOp::Add,
                Tok::Minus => BinOp::Sub,
                _ => break,
            };
            self.advance();
            let rhs = self.parse_mul()?;
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_mul(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Tok::Star => BinOp::Mul,
                Tok::Slash => BinOp::Div,
                _ => break,
            };
            self.advance();
            let rhs = self.parse_unary()?;
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<Expr> {
        match self.peek() {
            Tok::Bang => {
                self.advance();
                let e = self.parse_unary()?;
                Ok(Expr::Unary {
                    op: UnOp::Not,
                    expr: Box::new(e),
                })
            }
            Tok::Minus => {
                self.advance();
                let e = self.parse_unary()?;
                Ok(Expr::Unary {
                    op: UnOp::Neg,
                    expr: Box::new(e),
                })
            }
            _ => self.parse_primary(),
        }
    }

    fn parse_primary(&mut self) -> Result<Expr> {
        match self.peek().clone() {
            Tok::Str(s) => {
                self.advance();
                Ok(Expr::Str(s))
            }
            Tok::Num(n) => {
                self.advance();
                Ok(Expr::Num(n))
            }
            Tok::Duration(n, u) => {
                let span = self.peek_span();
                self.advance();
                let num: f64 = n
                    .parse()
                    .map_err(|_| VclError::new(span, format!("invalid number '{n}'")))?;
                let secs = ast::duration_secs(num, &u)
                    .ok_or_else(|| VclError::new(span, format!("unknown duration unit '{u}'")))?;
                Ok(Expr::Duration(secs))
            }
            Tok::Bytes(n, u) => {
                let span = self.peek_span();
                self.advance();
                let num: f64 = n
                    .parse()
                    .map_err(|_| VclError::new(span, format!("invalid number '{n}'")))?;
                let b = ast::bytes_val(num, &u)
                    .ok_or_else(|| VclError::new(span, format!("unknown bytes unit '{u}'")))?;
                Ok(Expr::Bytes(b))
            }
            Tok::CSource(s) => {
                self.advance();
                Ok(Expr::CSource(s))
            }
            Tok::Ident(s) if s == "true" => {
                self.advance();
                Ok(Expr::Bool(true))
            }
            Tok::Ident(s) if s == "false" => {
                self.advance();
                Ok(Expr::Bool(false))
            }
            Tok::LParen => {
                self.advance();
                let e = self.parse_expr()?;
                self.expect(Tok::RParen)?;
                Ok(e)
            }
            Tok::Ident(_) => {
                let (parts, _) = self.parse_dotted_ident()?;
                if matches!(self.peek(), Tok::LParen) {
                    self.advance();
                    let args = if matches!(self.peek(), Tok::RParen) {
                        Vec::new()
                    } else {
                        self.parse_arg_list()?
                    };
                    self.expect(Tok::RParen)?;
                    Ok(Expr::Call {
                        target: parts,
                        args,
                    })
                } else {
                    Ok(Expr::Var(parts))
                }
            }
            other => Err(self.error(format!("expected expression, found {other:?}"))),
        }
    }

    /// `container_lo` is the position right after the arg list's opening
    /// `(`, used to attach any comments preceding the first arg.
    fn parse_arg_list(&mut self) -> Result<Vec<Arg>> {
        let mut args = Vec::new();
        let mut prev: Option<Span> = None;
        loop {
            let next = self.peek_span();
            self.gap(prev, next.lo, Some(next));
            let a = self.parse_arg()?;
            prev = Some(a.span);
            args.push(a);
            if matches!(self.peek(), Tok::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        let container_hi = self.peek_span().lo; // ')' not yet consumed
        self.finish_gap(prev, container_hi);
        Ok(args)
    }

    /// `arg := IDENT '=' expr | expr` — lookahead: `IDENT '='` where the
    /// token after `=` is not itself `=` (so `f(x == 1)` isn't misparsed as
    /// a named argument).
    fn parse_arg(&mut self) -> Result<Arg> {
        let start = self.peek_span();
        if let Tok::Ident(name) = self.peek().clone() {
            if matches!(self.peek_at(1), Tok::Eq) && !matches!(self.peek_at(2), Tok::EqEq) {
                self.advance(); // ident
                self.advance(); // '='
                let value = self.parse_expr()?;
                let end = self.prev_span();
                return Ok(Arg {
                    name: Some(name),
                    value,
                    span: span_from(start, end),
                });
            }
        }
        let value = self.parse_expr()?;
        let end = self.prev_span();
        Ok(Arg {
            name: None,
            value,
            span: span_from(start, end),
        })
    }

    fn parse_expr_list(&mut self) -> Result<Vec<Expr>> {
        let mut exprs = vec![self.parse_expr()?];
        while matches!(self.peek(), Tok::Comma) {
            self.advance();
            exprs.push(self.parse_expr()?);
        }
        Ok(exprs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ast::builder::*;
    use pretty_assertions::assert_eq;

    fn prog(src: &str) -> Program {
        parse_str(src).expect("parse ok")
    }

    fn wrap(body: &str) -> String {
        format!("vcl 4.1;\n{body}\n")
    }

    // ───────────────────────── P1: top-level productions ─────────────────────────

    #[test]
    fn p1_import_plain_and_from() {
        let p = prog(&wrap(r#"import std;"#));
        assert_eq!(p.decls.len(), 1);
        match &p.decls[0] {
            Decl::Import { name, from, .. } => {
                assert_eq!(name, "std");
                assert_eq!(from, &None);
            }
            other => panic!("unexpected decl: {other:?}"),
        }

        let p2 = prog(&wrap(r#"import mymod from "path/to/mymod.so";"#));
        match &p2.decls[0] {
            Decl::Import { name, from, .. } => {
                assert_eq!(name, "mymod");
                assert_eq!(from.as_deref(), Some("path/to/mymod.so"));
            }
            other => panic!("unexpected decl: {other:?}"),
        }
    }

    #[test]
    fn p1_backend_body_and_none() {
        let p = prog(&wrap(
            r#"backend web { .host = "1.2.3.4"; .port = "80"; }
               backend b2 none;"#,
        ));
        match &p.decls[0] {
            Decl::Backend {
                name, none, body, ..
            } => {
                assert_eq!(name, "web");
                assert!(!*none);
                assert_eq!(body.as_ref().unwrap().len(), 2);
            }
            other => panic!("unexpected decl: {other:?}"),
        }
        match &p.decls[1] {
            Decl::Backend {
                name, none, body, ..
            } => {
                assert_eq!(name, "b2");
                assert!(*none);
                assert!(body.is_none());
            }
            other => panic!("unexpected decl: {other:?}"),
        }
    }

    #[test]
    fn p1_probe() {
        let p = prog(&wrap(
            r#"probe healthcheck { .url = "/healthz"; .interval = 5s; }"#,
        ));
        match &p.decls[0] {
            Decl::Probe { name, body, .. } => {
                assert_eq!(name, "healthcheck");
                assert_eq!(body.len(), 2);
            }
            other => panic!("unexpected decl: {other:?}"),
        }
    }

    #[test]
    fn p1_acl() {
        let p = prog(&wrap(r#"acl office { "10.0.0.0"/8; !"192.168.1.1"; }"#));
        match &p.decls[0] {
            Decl::Acl { name, entries, .. } => {
                assert_eq!(name, "office");
                assert_eq!(entries.len(), 2);
                assert_eq!(entries[0].addr, "10.0.0.0");
                assert_eq!(entries[0].mask, Some(8));
                assert!(!entries[0].negated);
                assert_eq!(entries[1].addr, "192.168.1.1");
                assert_eq!(entries[1].mask, None);
                assert!(entries[1].negated);
            }
            other => panic!("unexpected decl: {other:?}"),
        }
    }

    #[test]
    fn p1_sub() {
        let p = prog(&wrap(r#"sub vcl_recv { return (lookup); }"#));
        match &p.decls[0] {
            Decl::Sub { name, body, .. } => {
                assert_eq!(name, "vcl_recv");
                assert_eq!(body.len(), 1);
            }
            other => panic!("unexpected decl: {other:?}"),
        }
    }

    // ───────────────────────── P2: statement productions ─────────────────────────

    #[test]
    fn p2_set_unset_call() {
        let p = prog(&wrap(
            r#"sub vcl_recv {
                 set req.http.x-foo = "bar";
                 unset req.http.x-foo;
                 call custom_sub;
               }"#,
        ));
        let body = match &p.decls[0] {
            Decl::Sub { body, .. } => body,
            _ => panic!("expected sub"),
        };
        assert_eq!(body.len(), 3);
        assert!(
            matches!(&body[0], Stmt::Set { lhs, .. } if lhs.parts == vec!["req","http","x-foo"])
        );
        assert!(
            matches!(&body[1], Stmt::Unset { lhs, .. } if lhs.parts == vec!["req","http","x-foo"])
        );
        assert!(matches!(&body[2], Stmt::Call { sub, .. } if sub == "custom_sub"));
    }

    #[test]
    fn p2_return_variants() {
        let p = prog(&wrap(
            r#"sub vcl_recv {
                 return;
                 return (pass);
                 return (synth(404, "x"));
                 return (vcl(lbl));
               }"#,
        ));
        let body = match &p.decls[0] {
            Decl::Sub { body, .. } => body,
            _ => panic!("expected sub"),
        };
        assert_eq!(body.len(), 4);
        assert!(matches!(&body[0], Stmt::Return { action: None, .. }));
        assert!(
            matches!(&body[1], Stmt::Return { action: Some(a), .. } if a.name == "pass" && a.args.is_empty())
        );
        match &body[2] {
            Stmt::Return {
                action: Some(a), ..
            } => {
                assert_eq!(a.name, "synth");
                assert_eq!(a.args.len(), 2);
                assert!(matches!(&a.args[0], Expr::Num(n) if n == "404"));
                assert!(matches!(&a.args[1], Expr::Str(s) if s == "x"));
            }
            other => panic!("unexpected: {other:?}"),
        }
        match &body[3] {
            Stmt::Return {
                action: Some(a), ..
            } => {
                assert_eq!(a.name, "vcl");
                assert_eq!(a.args.len(), 1);
                assert!(
                    matches!(&a.args[0], Expr::Var(parts) if parts == &vec!["lbl".to_string()])
                );
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn p2_synthetic_new_bare_call() {
        let p = prog(&wrap(
            r#"sub vcl_init {
                 new counter = std.counter();
               }
               sub vcl_synth {
                 synthetic("hello");
                 std.log("logged");
               }"#,
        ));
        let init_body = match &p.decls[0] {
            Decl::Sub { body, .. } => body,
            _ => panic!("expected sub"),
        };
        match &init_body[0] {
            Stmt::New {
                name,
                vmod,
                ctor,
                args,
                ..
            } => {
                assert_eq!(name, "counter");
                assert_eq!(vmod, "std");
                assert_eq!(ctor, "counter");
                assert!(args.is_empty());
            }
            other => panic!("unexpected: {other:?}"),
        }
        let synth_body = match &p.decls[1] {
            Decl::Sub { body, .. } => body,
            _ => panic!("expected sub"),
        };
        assert!(
            matches!(&synth_body[0], Stmt::Synthetic { value: Expr::Str(s), .. } if s == "hello")
        );
        match &synth_body[1] {
            Stmt::Expr {
                expr: Expr::Call { target, args },
                ..
            } => {
                assert_eq!(target, &vec!["std".to_string(), "log".to_string()]);
                assert_eq!(args.len(), 1);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    // ───────────────────────── P3: elsif/elseif/else-if ─────────────────────────

    #[test]
    fn p3_elsif_spellings_identical_ast() {
        let a = prog(&wrap(
            r#"sub vcl_recv {
                 if (req.http.a) { set req.http.x = "1"; }
                 elsif (req.http.b) { set req.http.x = "2"; }
                 else { set req.http.x = "3"; }
               }"#,
        ));
        let b = prog(&wrap(
            r#"sub vcl_recv {
                 if (req.http.a) { set req.http.x = "1"; }
                 elseif (req.http.b) { set req.http.x = "2"; }
                 else { set req.http.x = "3"; }
               }"#,
        ));
        let c = prog(&wrap(
            r#"sub vcl_recv {
                 if (req.http.a) { set req.http.x = "1"; }
                 else if (req.http.b) { set req.http.x = "2"; }
                 else { set req.http.x = "3"; }
               }"#,
        ));

        fn norm(p: &Program) -> &Stmt {
            match &p.decls[0] {
                Decl::Sub { body, .. } => &body[0],
                _ => panic!("expected sub"),
            }
        }
        let (sa, sb, sc) = (norm(&a), norm(&b), norm(&c));
        match (sa, sb, sc) {
            (
                Stmt::If {
                    arms: aa,
                    else_body: ea,
                    ..
                },
                Stmt::If {
                    arms: ab,
                    else_body: eb,
                    ..
                },
                Stmt::If {
                    arms: ac,
                    else_body: ec,
                    ..
                },
            ) => {
                assert_eq!(aa.len(), 2);
                assert_eq!(ab.len(), 2);
                assert_eq!(ac.len(), 2);
                assert!(ea.is_some() && eb.is_some() && ec.is_some());
            }
            _ => panic!("expected If everywhere"),
        }
    }

    #[test]
    fn p3_chain_of_three_arms_plus_else() {
        let p = prog(&wrap(
            r#"sub vcl_recv {
                 if (a) { }
                 elsif (b) { }
                 elsif (c) { }
                 else { }
               }"#,
        ));
        match &p.decls[0] {
            Decl::Sub { body, .. } => match &body[0] {
                Stmt::If {
                    arms, else_body, ..
                } => {
                    assert_eq!(arms.len(), 3);
                    assert!(else_body.is_some());
                }
                other => panic!("unexpected: {other:?}"),
            },
            _ => panic!("expected sub"),
        }
    }

    // ───────────────────────── P4: precedence ─────────────────────────

    #[test]
    fn p4_precedence_or_and() {
        // a || b && c -> Or(a, And(b, c))
        let e = parse_expr_from_str("a || b && c");
        let expected = bin(
            ast::BinOp::Or,
            var(&["a"]),
            bin(ast::BinOp::And, var(&["b"]), var(&["c"])),
        );
        assert_eq!(format!("{e:?}"), format!("{expected:?}"));
    }

    #[test]
    fn p4_precedence_add_mul() {
        // 1 + 2 * 3 -> Add(1, Mul(2,3))
        let e = parse_expr_from_str("1 + 2 * 3");
        let expected = bin(
            ast::BinOp::Add,
            num("1"),
            bin(ast::BinOp::Mul, num("2"), num("3")),
        );
        assert_eq!(format!("{e:?}"), format!("{expected:?}"));
    }

    #[test]
    fn p4_unary_binding() {
        let e = parse_expr_from_str("!a == b");
        // '!' binds tighter than '==': Eq(Not(a), b)
        let expected = bin(ast::BinOp::Eq, un(ast::UnOp::Not, var(&["a"])), var(&["b"]));
        assert_eq!(format!("{e:?}"), format!("{expected:?}"));

        let e2 = parse_expr_from_str("-1 + 2");
        let expected2 = bin(ast::BinOp::Add, un(ast::UnOp::Neg, num("1")), num("2"));
        assert_eq!(format!("{e2:?}"), format!("{expected2:?}"));
    }

    fn parse_expr_from_str(e: &str) -> Expr {
        let src = wrap(&format!("sub vcl_recv {{ if ({e}) {{ }} }}"));
        let p = prog(&src);
        match &p.decls[0] {
            Decl::Sub { body, .. } => match &body[0] {
                Stmt::If { arms, .. } => arms[0].0.clone(),
                other => panic!("unexpected: {other:?}"),
            },
            _ => panic!("expected sub"),
        }
    }

    // ───────────────────────── P5: non-associative comparison ─────────────────────────

    #[test]
    fn p5_comparison_non_associative_is_error() {
        let src = wrap("sub vcl_recv { if (a < b < c) { } }");
        let err = parse_str(&src).expect_err("expected parse error");
        assert!(
            err.msg.to_lowercase().contains("non-associative")
                || err.msg.to_lowercase().contains("comparison")
        );
    }

    // ───────────────────────── P6: named args ─────────────────────────

    #[test]
    fn p6_named_args_and_no_misparse_on_eqeq() {
        let e = parse_expr_from_str_call("f(a, x=1)");
        match e {
            Expr::Call { target, args } => {
                assert_eq!(target, vec!["f".to_string()]);
                assert_eq!(args.len(), 2);
                assert_eq!(args[0].name, None);
                assert_eq!(args[1].name, Some("x".into()));
            }
            other => panic!("unexpected: {other:?}"),
        }

        let e2 = parse_expr_from_str_call("f(x == 1)");
        match e2 {
            Expr::Call { target, args } => {
                assert_eq!(target, vec!["f".to_string()]);
                assert_eq!(args.len(), 1);
                assert_eq!(args[0].name, None);
                assert!(matches!(
                    &args[0].value,
                    Expr::Binary {
                        op: ast::BinOp::Eq,
                        ..
                    }
                ));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    fn parse_expr_from_str_call(e: &str) -> Expr {
        let src = wrap(&format!("sub vcl_recv {{ set req.http.x = {e}; }}"));
        let p = prog(&src);
        match &p.decls[0] {
            Decl::Sub { body, .. } => match &body[0] {
                Stmt::Set { rhs, .. } => rhs.clone(),
                other => panic!("unexpected: {other:?}"),
            },
            _ => panic!("expected sub"),
        }
    }

    // ───────────────────────── P7: field values ─────────────────────────

    #[test]
    fn p7_inline_probe_ref_stringlist() {
        // Note: no ';' after the inline probe block's closing '}' -- unlike
        // every other field-value form, real VCC rejects one there
        // (confirmed against `varnishd -C`).
        let p = prog(&wrap(
            r#"probe named_probe { .url = "/"; }
               backend b1 {
                 .host = "1.2.3.4";
                 .probe = { .url = "/health"; }
               }
               backend b2 {
                 .host = "1.2.3.5";
                 .probe = named_probe;
               }
               backend b3 {
                 .request = "GET / HTTP/1.1" "Host: x";
               }"#,
        ));
        match &p.decls[1] {
            Decl::Backend { body, .. } => {
                let fields = body.as_ref().unwrap();
                match &fields[1].value {
                    FieldValue::Probe(inner) => assert_eq!(inner.len(), 1),
                    other => panic!("unexpected: {other:?}"),
                }
            }
            _ => panic!("expected backend"),
        }
        match &p.decls[2] {
            Decl::Backend { body, .. } => {
                let fields = body.as_ref().unwrap();
                match &fields[1].value {
                    FieldValue::ProbeRef(name) => assert_eq!(name, "named_probe"),
                    other => panic!("unexpected: {other:?}"),
                }
            }
            _ => panic!("expected backend"),
        }
        match &p.decls[3] {
            Decl::Backend { body, .. } => {
                let fields = body.as_ref().unwrap();
                match &fields[0].value {
                    FieldValue::StringList(strs) => {
                        assert_eq!(
                            strs,
                            &vec!["GET / HTTP/1.1".to_string(), "Host: x".to_string()]
                        )
                    }
                    other => panic!("unexpected: {other:?}"),
                }
            }
            _ => panic!("expected backend"),
        }
    }

    #[test]
    fn p7b_inline_probe_trailing_semicolon_is_a_parse_error() {
        // Real VCC rejects a ';' after an inline probe block's closing '}'
        // (unlike every other field-value form) -- our grammar must match.
        let src = wrap(
            r#"backend b1 {
                 .host = "1.2.3.4";
                 .probe = { .url = "/health"; };
               }"#,
        );
        assert!(
            parse_str(&src).is_err(),
            "trailing ';' after inline probe block should be rejected"
        );
    }

    // ───────────────────────── P8: error locations ─────────────────────────

    #[test]
    fn p8_errors_carry_correct_location() {
        // missing ';'
        let src = "vcl 4.1;\nsub vcl_recv {\n  set req.http.x = \"y\"\n}\n";
        let err = parse_str(src).expect_err("expected error");
        assert!(err.span.is_some());

        // unclosed brace
        let src2 = "vcl 4.1;\nsub vcl_recv {\n  set req.http.x = \"y\";\n";
        let err2 = parse_str(src2).expect_err("expected error");
        assert!(err2.span.is_some());

        // vcl 4.0 rejected
        let src3 = "vcl 4.0;\n";
        let err3 = parse_str(src3).expect_err("expected error");
        assert!(err3.msg.contains("only VCL 4.1 supported"));
    }

    // ───────────────────────── P9: every span non-dummy ─────────────────────────

    #[test]
    fn p9_all_spans_non_dummy() {
        let p = prog(&wrap(
            r#"
               import std;
               probe hc { .url = "/"; }
               backend web {
                 .host = "1.2.3.4";
                 .probe = hc;
               }
               acl office { "10.0.0.0"/8; }
               sub vcl_recv {
                 set req.http.x = "1";
                 unset req.http.x;
                 call vcl_recv;
                 if (req.url == "/") {
                   return (lookup);
                 } else {
                   return (pass);
                 }
               }
            "#,
        ));
        assert_span_ok(p.vcl_version);
        for d in &p.decls {
            assert_span_ok(d.span());
            match d {
                Decl::Backend {
                    body: Some(fields), ..
                }
                | Decl::Probe { body: fields, .. } => {
                    for f in fields {
                        assert_span_ok(f.span);
                    }
                }
                Decl::Acl { entries, .. } => {
                    for e in entries {
                        assert_span_ok(e.span);
                    }
                }
                Decl::Sub { body, .. } => {
                    for s in body {
                        check_stmt_span(s);
                    }
                }
                _ => {}
            }
        }
    }

    // ───────────────────────── P10: secondary vcl declarations ─────────────────────────

    #[test]
    fn p10a_secondary_vcl_decl_matching_version() {
        // A secondary vcl 4.1; (matching version) in mid-stream should be
        // tolerated and discarded, not appearing in Program.decls
        let src = wrap(
            r#"backend b1 none;
               vcl 4.1;
               backend b2 none;"#,
        );
        let p = prog(&src);
        // Should have 2 backend decls, not 3 (no vcl decl in the list)
        assert_eq!(p.decls.len(), 2);
        match &p.decls[0] {
            Decl::Backend { name, .. } => assert_eq!(name, "b1"),
            _ => panic!("expected backend"),
        }
        match &p.decls[1] {
            Decl::Backend { name, .. } => assert_eq!(name, "b2"),
            _ => panic!("expected backend"),
        }
    }

    #[test]
    fn p10b_secondary_vcl_decl_mismatched_version_still_accepted() {
        // Per verified fact 1: a secondary vcl with mismatched version (e.g. 4.0)
        // is still accepted and silently discarded (no error).
        // This matches real Varnish behavior.
        let src = wrap(
            r#"backend b1 none;
               vcl 4.0;
               backend b2 none;"#,
        );
        let p = prog(&src);
        // Should parse OK and have 2 backend decls
        assert_eq!(p.decls.len(), 2);
        match &p.decls[0] {
            Decl::Backend { name, .. } => assert_eq!(name, "b1"),
            _ => panic!("expected backend"),
        }
        match &p.decls[1] {
            Decl::Backend { name, .. } => assert_eq!(name, "b2"),
            _ => panic!("expected backend"),
        }
    }

    #[test]
    fn p10c_secondary_vcl_decl_malformed_is_error() {
        // A malformed secondary vcl decl (e.g. vcl garbage;) should still error
        // -- the shape is enforced, just not the version check.
        let src = wrap(
            r#"backend b1 none;
               vcl garbage;
               backend b2 none;"#,
        );
        let result = parse_str(&src);
        assert!(
            result.is_err(),
            "malformed secondary vcl decl (vcl garbage;) should error"
        );
        let err = result.unwrap_err();
        // Should complain about expected version number
        assert!(
            err.msg.contains("expected VCL version number")
                || err.msg.contains("expected version minor number"),
            "error message: {}",
            err.msg
        );
    }

    fn check_stmt_span(s: &Stmt) {
        assert_span_ok(s.span());
        if let Stmt::If {
            arms, else_body, ..
        } = s
        {
            for (_, body) in arms {
                for st in body {
                    check_stmt_span(st);
                }
            }
            if let Some(eb) = else_body {
                for st in eb {
                    check_stmt_span(st);
                }
            }
        }
    }

    fn assert_span_ok(sp: Span) {
        // A dummy span is file:0 lo:0 hi:0; any real token has hi > lo (or at
        // least is not the exact dummy sentinel) and resolves cleanly.
        assert_ne!(
            (sp.lo, sp.hi),
            (0, 0),
            "span should not be the dummy sentinel"
        );
    }
}
