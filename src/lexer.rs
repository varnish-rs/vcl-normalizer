//! Hand-written lexer with `include` splicing (see spec §2).
//!
//! The lexer works in two layers:
//!   - [`lex_single_file`] turns one file's source text into a flat token
//!     stream (no `include` handling).
//!   - [`splice_file`] recursively lexes a file, then rewrites the token
//!     stream, replacing every `include "path";` triple with the spliced
//!     token stream of the included file (itself recursively spliced).
//!
//! [`lex`] is the public entry point used by the parser; [`lex_str`] is a
//! convenience for tests (no real file on disk, no includes possible except
//! via explicit `include_dirs`).

use crate::ast::{Result, SourceMap, Span, VclError};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Ident(String),
    Num(String),
    Duration(String, String), // (number text, unit)
    Bytes(String, String),    // (number text, unit)
    Str(String),
    CSource(String),
    LBrace,
    RBrace,
    LParen,
    RParen,
    Semi,
    Comma,
    Dot,
    Eq,
    EqEq,
    Neq,
    Tilde,
    NTilde,
    Lt,
    Leq,
    Gt,
    Geq,
    AndAnd,
    OrOr,
    Bang,
    Plus,
    Minus,
    Star,
    Slash,
    Eof,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub tok: Tok,
    pub span: Span,
}

fn mk(tok: Tok, file: u32, lo: usize, hi: usize) -> Token {
    Token {
        tok,
        span: Span {
            file,
            lo: lo as u32,
            hi: hi as u32,
        },
    }
}

const DURATION_UNITS_2: &[&str] = &["ms"];
const DURATION_UNITS_1: &[&str] = &["s", "m", "h", "d", "w", "y"];
const BYTES_UNITS_2: &[&str] = &["KB", "MB", "GB", "TB"];
const BYTES_UNITS_1: &[&str] = &["B"];

enum UnitKind {
    Duration,
    Bytes,
}

fn try_match_unit(bytes: &[u8], pos: usize, len: usize) -> Option<(String, usize, UnitKind)> {
    if pos + 2 <= len {
        if let Ok(two) = std::str::from_utf8(&bytes[pos..pos + 2]) {
            if DURATION_UNITS_2.contains(&two) {
                return Some((two.to_string(), 2, UnitKind::Duration));
            }
            if BYTES_UNITS_2.contains(&two) {
                return Some((two.to_string(), 2, UnitKind::Bytes));
            }
        }
    }
    if pos < len {
        if let Ok(one) = std::str::from_utf8(&bytes[pos..pos + 1]) {
            if DURATION_UNITS_1.contains(&one) {
                return Some((one.to_string(), 1, UnitKind::Duration));
            }
            if BYTES_UNITS_1.contains(&one) {
                return Some((one.to_string(), 1, UnitKind::Bytes));
            }
        }
    }
    None
}

/// Lex a single file's contents (no include handling) into a flat token
/// stream. Does NOT append an `Eof` token — callers (splicing / top level)
/// decide where the final `Eof` goes.
fn lex_single_file(content: &str, file_id: u32) -> Result<Vec<Token>> {
    let bytes = content.as_bytes();
    let len = bytes.len();
    let mut pos = 0usize;
    let mut toks = Vec::new();

    loop {
        // Skip whitespace + comments.
        loop {
            if pos >= len {
                break;
            }
            let c = bytes[pos];
            if c == b' ' || c == b'\t' || c == b'\r' || c == b'\n' {
                pos += 1;
                continue;
            }
            if c == b'#' {
                while pos < len && bytes[pos] != b'\n' {
                    pos += 1;
                }
                continue;
            }
            if c == b'/' && pos + 1 < len && bytes[pos + 1] == b'/' {
                pos += 2;
                while pos < len && bytes[pos] != b'\n' {
                    pos += 1;
                }
                continue;
            }
            if c == b'/' && pos + 1 < len && bytes[pos + 1] == b'*' {
                let start = pos;
                pos += 2;
                let mut closed = false;
                while pos + 1 < len {
                    if bytes[pos] == b'*' && bytes[pos + 1] == b'/' {
                        pos += 2;
                        closed = true;
                        break;
                    }
                    pos += 1;
                }
                if !closed {
                    return Err(VclError::new(
                        Span {
                            file: file_id,
                            lo: start as u32,
                            hi: len as u32,
                        },
                        "unterminated block comment",
                    ));
                }
                continue;
            }
            break;
        }

        if pos >= len {
            break;
        }

        let start = pos;
        let c = bytes[pos];

        // Inline C source: `C{ ... }C`, no whitespace between C and {.
        if c == b'C' && pos + 1 < len && bytes[pos + 1] == b'{' {
            pos += 2;
            let content_start = pos;
            let mut found = false;
            while pos + 1 < len {
                if bytes[pos] == b'}' && bytes[pos + 1] == b'C' {
                    found = true;
                    break;
                }
                pos += 1;
            }
            if !found {
                return Err(VclError::new(
                    Span {
                        file: file_id,
                        lo: start as u32,
                        hi: len as u32,
                    },
                    "unterminated C{ ... }C block",
                ));
            }
            let raw = &content[content_start..pos];
            pos += 2; // consume "}C"
            toks.push(mk(Tok::CSource(raw.to_string()), file_id, start, pos));
            continue;
        }

        // Identifiers (allow '-' inside, like VCC).
        if c.is_ascii_alphabetic() || c == b'_' {
            pos += 1;
            while pos < len {
                let d = bytes[pos];
                if d.is_ascii_alphanumeric() || d == b'_' || d == b'-' {
                    pos += 1;
                } else {
                    break;
                }
            }
            let text = &content[start..pos];
            toks.push(mk(Tok::Ident(text.to_string()), file_id, start, pos));
            continue;
        }

        // Numbers, possibly with a duration/bytes unit suffix.
        if c.is_ascii_digit() {
            pos += 1;
            while pos < len && bytes[pos].is_ascii_digit() {
                pos += 1;
            }
            if pos < len && bytes[pos] == b'.' && pos + 1 < len && bytes[pos + 1].is_ascii_digit() {
                pos += 1;
                while pos < len && bytes[pos].is_ascii_digit() {
                    pos += 1;
                }
            }
            let num_end = pos;
            let num_text = content[start..num_end].to_string();
            match try_match_unit(bytes, pos, len) {
                Some((unit, unit_len, kind)) => {
                    pos += unit_len;
                    let tok = match kind {
                        UnitKind::Duration => Tok::Duration(num_text, unit),
                        UnitKind::Bytes => Tok::Bytes(num_text, unit),
                    };
                    toks.push(mk(tok, file_id, start, pos));
                }
                None => {
                    toks.push(mk(Tok::Num(num_text), file_id, start, num_end));
                }
            }
            continue;
        }

        // Strings: "...", {"..."}, """...""".
        if c == b'"' {
            if pos + 2 < len && bytes[pos + 1] == b'"' && bytes[pos + 2] == b'"' {
                // Triple-quoted long string.
                pos += 3;
                let content_start = pos;
                let mut found = false;
                while pos + 3 <= len {
                    if bytes[pos] == b'"' && bytes[pos + 1] == b'"' && bytes[pos + 2] == b'"' {
                        found = true;
                        break;
                    }
                    pos += 1;
                }
                if !found {
                    return Err(VclError::new(
                        Span {
                            file: file_id,
                            lo: start as u32,
                            hi: len as u32,
                        },
                        "unterminated \"\"\"...\"\"\" string",
                    ));
                }
                let raw = content[content_start..pos].to_string();
                pos += 3;
                toks.push(mk(Tok::Str(raw), file_id, start, pos));
                continue;
            }
            // Simple string: no escapes; may not contain '"' or newline.
            pos += 1;
            let content_start = pos;
            let mut found = false;
            while pos < len {
                if bytes[pos] == b'"' {
                    found = true;
                    break;
                }
                if bytes[pos] == b'\n' {
                    break;
                }
                pos += 1;
            }
            if !found {
                return Err(VclError::new(
                    Span {
                        file: file_id,
                        lo: start as u32,
                        hi: pos as u32,
                    },
                    "unterminated string (contains newline or runs to EOF)",
                ));
            }
            let raw = content[content_start..pos].to_string();
            pos += 1; // consume closing quote
            toks.push(mk(Tok::Str(raw), file_id, start, pos));
            continue;
        }

        // '{' — either LBrace or the start of a {"..."} long string.
        if c == b'{' {
            if pos + 1 < len && bytes[pos + 1] == b'"' {
                pos += 2;
                let content_start = pos;
                let mut found = false;
                while pos + 1 < len {
                    if bytes[pos] == b'"' && bytes[pos + 1] == b'}' {
                        found = true;
                        break;
                    }
                    pos += 1;
                }
                if !found {
                    return Err(VclError::new(
                        Span {
                            file: file_id,
                            lo: start as u32,
                            hi: len as u32,
                        },
                        "unterminated {\"...\"} string",
                    ));
                }
                let raw = content[content_start..pos].to_string();
                pos += 2; // consume "}
                toks.push(mk(Tok::Str(raw), file_id, start, pos));
                continue;
            }
            toks.push(mk(Tok::LBrace, file_id, start, start + 1));
            pos += 1;
            continue;
        }

        macro_rules! two_or_one {
            ($second:expr, $two:expr, $one:expr) => {{
                if pos + 1 < len && bytes[pos + 1] == $second {
                    let t = mk($two, file_id, start, start + 2);
                    pos += 2;
                    t
                } else {
                    let t = mk($one, file_id, start, start + 1);
                    pos += 1;
                    t
                }
            }};
        }

        let tok = match c {
            b'}' => {
                pos += 1;
                mk(Tok::RBrace, file_id, start, pos)
            }
            b'(' => {
                pos += 1;
                mk(Tok::LParen, file_id, start, pos)
            }
            b')' => {
                pos += 1;
                mk(Tok::RParen, file_id, start, pos)
            }
            b';' => {
                pos += 1;
                mk(Tok::Semi, file_id, start, pos)
            }
            b',' => {
                pos += 1;
                mk(Tok::Comma, file_id, start, pos)
            }
            b'.' => {
                pos += 1;
                mk(Tok::Dot, file_id, start, pos)
            }
            b'=' => two_or_one!(b'=', Tok::EqEq, Tok::Eq),
            b'!' => {
                if pos + 1 < len && bytes[pos + 1] == b'=' {
                    let t = mk(Tok::Neq, file_id, start, start + 2);
                    pos += 2;
                    t
                } else if pos + 1 < len && bytes[pos + 1] == b'~' {
                    let t = mk(Tok::NTilde, file_id, start, start + 2);
                    pos += 2;
                    t
                } else {
                    let t = mk(Tok::Bang, file_id, start, start + 1);
                    pos += 1;
                    t
                }
            }
            b'~' => {
                pos += 1;
                mk(Tok::Tilde, file_id, start, pos)
            }
            b'<' => two_or_one!(b'=', Tok::Leq, Tok::Lt),
            b'>' => two_or_one!(b'=', Tok::Geq, Tok::Gt),
            b'&' => {
                if pos + 1 < len && bytes[pos + 1] == b'&' {
                    let t = mk(Tok::AndAnd, file_id, start, start + 2);
                    pos += 2;
                    t
                } else {
                    return Err(VclError::new(
                        Span {
                            file: file_id,
                            lo: start as u32,
                            hi: (start + 1) as u32,
                        },
                        "unexpected character '&' (did you mean '&&'?)",
                    ));
                }
            }
            b'|' => {
                if pos + 1 < len && bytes[pos + 1] == b'|' {
                    let t = mk(Tok::OrOr, file_id, start, start + 2);
                    pos += 2;
                    t
                } else {
                    return Err(VclError::new(
                        Span {
                            file: file_id,
                            lo: start as u32,
                            hi: (start + 1) as u32,
                        },
                        "unexpected character '|' (did you mean '||'?)",
                    ));
                }
            }
            b'+' => {
                pos += 1;
                mk(Tok::Plus, file_id, start, pos)
            }
            b'-' => {
                pos += 1;
                mk(Tok::Minus, file_id, start, pos)
            }
            b'*' => {
                pos += 1;
                mk(Tok::Star, file_id, start, pos)
            }
            b'/' => {
                pos += 1;
                mk(Tok::Slash, file_id, start, pos)
            }
            other => {
                return Err(VclError::new(
                    Span {
                        file: file_id,
                        lo: start as u32,
                        hi: (start + 1) as u32,
                    },
                    format!("unexpected character {:?}", other as char),
                ));
            }
        };
        toks.push(tok);
    }

    Ok(toks)
}

fn resolve_include(raw: &str, including_file: &Path, include_dirs: &[PathBuf]) -> Option<PathBuf> {
    let base = including_file
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let candidate = base.join(raw);
    if candidate.exists() {
        return Some(candidate);
    }
    for d in include_dirs {
        let candidate = d.join(raw);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

const MAX_INCLUDE_DEPTH: usize = 32;

/// Recursively lex `path`, splicing in the token streams of any
/// `include "...";` statements found in it. Returns the tokens (no `Eof`)
/// plus the file id assigned to `path` itself.
fn splice_file(
    path: &Path,
    include_dirs: &[PathBuf],
    sm: &mut SourceMap,
    stack: &mut Vec<PathBuf>,
    include_span: Option<Span>,
) -> Result<(Vec<Token>, u32)> {
    if stack.len() >= MAX_INCLUDE_DEPTH {
        let span = include_span.unwrap_or(Span::dummy());
        return Err(VclError::new(
            span,
            format!("include depth exceeds {MAX_INCLUDE_DEPTH}"),
        ));
    }

    let canon = path.canonicalize().map_err(|e| {
        let span = include_span.unwrap_or(Span::dummy());
        VclError::new(span, format!("cannot open '{}': {e}", path.display()))
    })?;

    if let Some(pos) = stack.iter().position(|p| p == &canon) {
        let cycle: Vec<String> = stack[pos..]
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        let span = include_span.unwrap_or(Span::dummy());
        return Err(VclError::new(
            span,
            format!(
                "circular include: {} -> {}",
                cycle.join(" -> "),
                canon.display()
            ),
        ));
    }

    let content = fs::read_to_string(path).map_err(|e| {
        let span = include_span.unwrap_or(Span::dummy());
        VclError::new(span, format!("cannot read '{}': {e}", path.display()))
    })?;

    let file_id = sm.add(path.to_path_buf(), content.clone());
    let raw = lex_single_file(&content, file_id)?;

    stack.push(canon);

    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        let is_include = matches!(&raw[i].tok, Tok::Ident(s) if s == "include")
            && i + 2 < raw.len()
            && matches!(&raw[i + 1].tok, Tok::Str(_))
            && matches!(&raw[i + 2].tok, Tok::Semi);
        if is_include {
            let inc_path_str = if let Tok::Str(s) = &raw[i + 1].tok {
                s.clone()
            } else {
                unreachable!()
            };
            let inc_span = raw[i + 1].span;
            let resolved = resolve_include(&inc_path_str, path, include_dirs).ok_or_else(|| {
                VclError::new(inc_span, format!("include file not found: {inc_path_str}"))
            })?;
            let (spliced, _child_id) =
                splice_file(&resolved, include_dirs, sm, stack, Some(inc_span))?;
            out.extend(spliced);
            i += 3;
        } else {
            out.push(raw[i].clone());
            i += 1;
        }
    }

    stack.pop();
    Ok((out, file_id))
}

/// Lex `path` (and splice in all its includes), returning the full token
/// stream (terminated by `Eof`) plus the populated `SourceMap`.
pub fn lex(path: &Path, include_dirs: &[PathBuf]) -> Result<(Vec<Token>, SourceMap)> {
    let mut sm = SourceMap::default();
    let mut stack = Vec::new();
    let (mut tokens, root_id) = splice_file(path, include_dirs, &mut sm, &mut stack, None)?;
    let end = sm.files[root_id as usize].1.len() as u32;
    tokens.push(Token {
        tok: Tok::Eof,
        span: Span {
            file: root_id,
            lo: end,
            hi: end,
        },
    });
    Ok((tokens, sm))
}

// ─────────────────────────── VCL.SHOW integration ───────────────────────────

#[allow(dead_code)]
struct VclShowState<'a> {
    chunks: &'a [crate::vclshow::VclShowChunk],
    next: usize, // index of the next not-yet-consumed chunk
}

/// Recursively lex a chunk from a VCL.SHOW dump, splicing in the token
/// streams of any `include "...";` statements found in it (by consuming
/// subsequent chunks in order, not by looking at the include path string).
/// Returns the tokens (no `Eof`) plus the file id assigned to this chunk.
#[allow(dead_code)]
fn splice_chunk(
    chunk_idx: usize,
    state: &mut VclShowState,
    sm: &mut SourceMap,
    _include_span: Option<Span>,
) -> Result<(Vec<Token>, u32)> {
    let chunk = &state.chunks[chunk_idx];
    let file_id = sm.add(PathBuf::from(&chunk.filename), chunk.content.clone());
    let raw = lex_single_file(&chunk.content, file_id)?;

    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        let is_include = matches!(&raw[i].tok, Tok::Ident(s) if s == "include")
            && i + 2 < raw.len()
            && matches!(&raw[i + 1].tok, Tok::Str(_))
            && matches!(&raw[i + 2].tok, Tok::Semi);
        if is_include {
            let inc_span = raw[i + 1].span;
            if state.next >= state.chunks.len() {
                return Err(VclError::new(
                    inc_span,
                    "include statement has no corresponding VCL.SHOW chunk left (dump truncated or malformed)".to_string(),
                ));
            }
            let idx = state.next;
            state.next += 1;
            let (spliced, _child_id) = splice_chunk(idx, state, sm, Some(inc_span))?;
            out.extend(spliced);
            i += 3;
        } else {
            out.push(raw[i].clone());
            i += 1;
        }
    }

    Ok((out, file_id))
}

/// Entry point for lexing a VCL.SHOW dump: parses the chunks and splices
/// them based on the include structure recorded in the dump (pre-order DFS
/// order). Returns the full token stream (terminated by `Eof`) plus the
/// populated `SourceMap`.
pub fn lex_from_vcl_show(
    chunks: &[crate::vclshow::VclShowChunk],
) -> Result<(Vec<Token>, SourceMap)> {
    if chunks.is_empty() {
        return Err(VclError::new(
            Span::dummy(),
            "no VCL.SHOW chunks found in input".to_string(),
        ));
    }
    let mut sm = SourceMap::default();
    let mut state = VclShowState { chunks, next: 1 }; // chunk 0 is the root; the next chunk any include consumes is chunk 1
    let (mut tokens, root_id) = splice_chunk(0, &mut state, &mut sm, None)?;

    let leftover = &chunks[state.next..];
    match leftover {
        [] => {}
        [only] if only.filename == "Builtin" => {} // expected, silently dropped
        _ => {
            let names: Vec<&str> = leftover.iter().map(|c| c.filename.as_str()).collect();
            return Err(VclError::new(
                Span::dummy(),
                format!(
                    "{} unused VCL.SHOW chunk(s) after the include tree (expected at most a trailing 'Builtin' chunk): {}",
                    leftover.len(), names.join(", ")
                ),
            ));
        }
    }

    let end = sm.files[root_id as usize].1.len() as u32;
    tokens.push(Token {
        tok: Tok::Eof,
        span: Span {
            file: root_id,
            lo: end,
            hi: end,
        },
    });
    Ok((tokens, sm))
}

/// Convenience for tests: lex a string as a standalone virtual file (no
/// includes unless `include_dirs`-relative paths happen to resolve).
#[cfg(test)]
pub fn lex_str(src: &str) -> Result<(Vec<Token>, SourceMap)> {
    let mut sm = SourceMap::default();
    let file_id = sm.add(PathBuf::from("<test>"), src.to_string());
    let mut tokens = lex_single_file(src, file_id)?;
    let end = src.len() as u32;
    tokens.push(Token {
        tok: Tok::Eof,
        span: Span {
            file: file_id,
            lo: end,
            hi: end,
        },
    });
    Ok((tokens, sm))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(src: &str) -> Vec<Tok> {
        lex_str(src)
            .expect("lex ok")
            .0
            .into_iter()
            .map(|t| t.tok)
            .collect()
    }

    #[test]
    fn l1_comment_styles_stripped_but_not_in_strings() {
        let src = "# hash comment\nident1 // slash comment\nident2 /* block\ncomment */ ident3 \"# not a comment\"";
        let t = toks(src);
        assert_eq!(
            t,
            vec![
                Tok::Ident("ident1".into()),
                Tok::Ident("ident2".into()),
                Tok::Ident("ident3".into()),
                Tok::Str("# not a comment".into()),
                Tok::Eof,
            ]
        );
    }

    #[test]
    fn l2_three_string_forms_same_value_no_escapes() {
        let a = toks(r#""hello\nworld""#);
        let b = toks(r#"{"hello\nworld"}"#);
        let c = toks("\"\"\"hello\\nworld\"\"\"");
        // literal backslash-n, NOT a newline escape
        assert_eq!(a, vec![Tok::Str("hello\\nworld".into()), Tok::Eof]);
        assert_eq!(b, vec![Tok::Str("hello\\nworld".into()), Tok::Eof]);
        assert_eq!(c, vec![Tok::Str("hello\\nworld".into()), Tok::Eof]);
    }

    #[test]
    fn l3_long_string_terminators() {
        // {"..."} ends at first "}, may contain '"' and newlines
        let t = toks("{\"a \"quoted\" \n multiline\"}");
        assert_eq!(
            t,
            vec![Tok::Str("a \"quoted\" \n multiline".into()), Tok::Eof]
        );

        // """...""" form: ends at first """
        let t3 = toks("\"\"\"line1\nline2\"\"\"");
        assert_eq!(t3, vec![Tok::Str("line1\nline2".into()), Tok::Eof]);
    }

    #[test]
    fn l4_ident_vs_minus() {
        assert_eq!(toks("a-b"), vec![Tok::Ident("a-b".into()), Tok::Eof]);
        assert_eq!(
            toks("a - b"),
            vec![
                Tok::Ident("a".into()),
                Tok::Minus,
                Tok::Ident("b".into()),
                Tok::Eof
            ]
        );
        assert_eq!(
            toks("10-5"),
            vec![
                Tok::Num("10".into()),
                Tok::Minus,
                Tok::Num("5".into()),
                Tok::Eof
            ]
        );
    }

    #[test]
    fn l5_unit_suffixes() {
        assert_eq!(
            toks("1ms"),
            vec![Tok::Duration("1".into(), "ms".into()), Tok::Eof]
        );
        assert_eq!(
            toks("1s"),
            vec![Tok::Duration("1".into(), "s".into()), Tok::Eof]
        );
        assert_eq!(
            toks("1m"),
            vec![Tok::Duration("1".into(), "m".into()), Tok::Eof]
        );
        assert_eq!(
            toks("1h"),
            vec![Tok::Duration("1".into(), "h".into()), Tok::Eof]
        );
        assert_eq!(
            toks("1d"),
            vec![Tok::Duration("1".into(), "d".into()), Tok::Eof]
        );
        assert_eq!(
            toks("1w"),
            vec![Tok::Duration("1".into(), "w".into()), Tok::Eof]
        );
        assert_eq!(
            toks("1y"),
            vec![Tok::Duration("1".into(), "y".into()), Tok::Eof]
        );
        assert_eq!(
            toks("1B"),
            vec![Tok::Bytes("1".into(), "B".into()), Tok::Eof]
        );
        assert_eq!(
            toks("1KB"),
            vec![Tok::Bytes("1".into(), "KB".into()), Tok::Eof]
        );
        assert_eq!(
            toks("1MB"),
            vec![Tok::Bytes("1".into(), "MB".into()), Tok::Eof]
        );
        assert_eq!(
            toks("1GB"),
            vec![Tok::Bytes("1".into(), "GB".into()), Tok::Eof]
        );
        assert_eq!(
            toks("1TB"),
            vec![Tok::Bytes("1".into(), "TB".into()), Tok::Eof]
        );
        // longest-match: 1ms is one token, not "1m" + "s"
        assert_eq!(
            toks("1ms"),
            vec![Tok::Duration("1".into(), "ms".into()), Tok::Eof]
        );
        // decimal duration, single token
        assert_eq!(
            toks("1.5h"),
            vec![Tok::Duration("1.5".into(), "h".into()), Tok::Eof]
        );
    }

    #[test]
    fn l6_inline_c_verbatim() {
        let src = "C{\nint x = 1; /* not a vcl comment */\n}C";
        let t = toks(src);
        assert_eq!(
            t,
            vec![
                Tok::CSource("\nint x = 1; /* not a vcl comment */\n".into()),
                Tok::Eof
            ]
        );
    }

    #[test]
    fn l7_include_splice_relative_and_dash_i() {
        let dir =
            std::env::temp_dir().join(format!("vcl_normalizer_lex_test_l7_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let incdir = dir.join("inc");
        std::fs::create_dir_all(&incdir).unwrap();

        let main_path = dir.join("main.vcl");
        let rel_path = dir.join("relative.vcl");
        let idir_path = incdir.join("fromdir.vcl");

        std::fs::write(&rel_path, "ident_from_relative;").unwrap();
        std::fs::write(&idir_path, "ident_from_idir;").unwrap();
        std::fs::write(
            &main_path,
            "before include \"relative.vcl\"; include \"fromdir.vcl\"; after",
        )
        .unwrap();

        let (tokens, sm) = lex(&main_path, std::slice::from_ref(&incdir)).expect("lex ok");
        let kinds: Vec<Tok> = tokens.iter().map(|t| t.tok.clone()).collect();
        assert_eq!(
            kinds,
            vec![
                Tok::Ident("before".into()),
                Tok::Ident("ident_from_relative".into()),
                Tok::Semi,
                Tok::Ident("ident_from_idir".into()),
                Tok::Semi,
                Tok::Ident("after".into()),
                Tok::Eof,
            ]
        );

        // spliced tokens carry the included file's Span.file
        let rel_file_id = sm.files.iter().position(|(p, _)| p == &rel_path).unwrap() as u32;
        let idir_file_id = sm.files.iter().position(|(p, _)| p == &idir_path).unwrap() as u32;
        assert_eq!(tokens[1].span.file, rel_file_id);
        assert_eq!(tokens[3].span.file, idir_file_id);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn l8_circular_include_and_depth_cap() {
        let dir =
            std::env::temp_dir().join(format!("vcl_normalizer_lex_test_l8_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let a_path = dir.join("a.vcl");
        let b_path = dir.join("b.vcl");
        std::fs::write(&a_path, "include \"b.vcl\";").unwrap();
        std::fs::write(&b_path, "include \"a.vcl\";").unwrap();

        let err = lex(&a_path, &[]).expect_err("expected circular include error");
        assert!(err.msg.contains("circular"), "msg was: {}", err.msg);

        // depth cap: chain of 40 includes, each including the next
        let depth_dir = dir.join("depth");
        std::fs::create_dir_all(&depth_dir).unwrap();
        for i in 0..40 {
            let p = depth_dir.join(format!("f{i}.vcl"));
            let body = if i == 39 {
                "ident_leaf;".to_string()
            } else {
                format!("include \"f{}.vcl\";", i + 1)
            };
            std::fs::write(&p, body).unwrap();
        }
        let root = depth_dir.join("f0.vcl");
        let err2 = lex(&root, &[]).expect_err("expected depth cap error");
        assert!(err2.msg.contains("depth"), "msg was: {}", err2.msg);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn l9_span_offsets_and_line_col() {
        let src = "ab cd\nef";
        let (tokens, sm) = lex_str(src).expect("lex ok");
        // "ab" at 0..2, "cd" at 3..5, "ef" at 6..8
        assert_eq!(tokens[0].span.lo, 0);
        assert_eq!(tokens[0].span.hi, 2);
        assert_eq!(tokens[1].span.lo, 3);
        assert_eq!(tokens[1].span.hi, 5);
        assert_eq!(tokens[2].span.lo, 6);
        assert_eq!(tokens[2].span.hi, 8);

        let (_, line, col) = sm.resolve(tokens[2].span);
        assert_eq!(line, 2);
        assert_eq!(col, 1);
    }

    // ───────────────────── VCL.SHOW tests ─────────────────

    #[test]
    fn v1_single_chunk_no_includes() {
        // 1 chunk, no includes — tokens should match plain lex_single_file
        let content = "backend default none;";
        let chunks = vec![crate::vclshow::VclShowChunk {
            index: 0,
            declared_len: content.len() as u32,
            filename: "/etc/varnish/root.vcl".to_string(),
            content: content.to_string(),
        }];

        let (tokens, sm) = lex_from_vcl_show(&chunks).expect("lex ok");
        // Remove the Eof token to compare with plain lex_single_file
        let tokens_without_eof: Vec<Tok> = tokens[..tokens.len() - 1]
            .iter()
            .map(|t| t.tok.clone())
            .collect();

        let (plain_tokens, _) = lex_str(content).expect("plain lex ok");
        let plain_without_eof: Vec<Tok> = plain_tokens[..plain_tokens.len() - 1]
            .iter()
            .map(|t| t.tok.clone())
            .collect();

        assert_eq!(tokens_without_eof, plain_without_eof);
        assert!(matches!(tokens.last(), Some(t) if matches!(t.tok, Tok::Eof)));
        assert_eq!(sm.files.len(), 1);
        assert_eq!(sm.files[0].0.to_string_lossy(), "/etc/varnish/root.vcl");
    }

    #[test]
    fn v2_root_includes_one_child() {
        // Root includes 1 child (no grandchildren) — tokens spliced correctly, both get correct file ids
        let root_content = "include \"child.vcl\";";
        let child_content = "backend default none;";
        let chunks = vec![
            crate::vclshow::VclShowChunk {
                index: 0,
                declared_len: root_content.len() as u32,
                filename: "/etc/varnish/root.vcl".to_string(),
                content: root_content.to_string(),
            },
            crate::vclshow::VclShowChunk {
                index: 1,
                declared_len: child_content.len() as u32,
                filename: "/etc/varnish/child.vcl".to_string(),
                content: child_content.to_string(),
            },
        ];

        let (tokens, sm) = lex_from_vcl_show(&chunks).expect("lex ok");
        let toks: Vec<Tok> = tokens.iter().map(|t| t.tok.clone()).collect();

        // After splicing, we should have tokens from child, then Eof (include is replaced)
        assert_eq!(
            toks,
            vec![
                Tok::Ident("backend".into()),
                Tok::Ident("default".into()),
                Tok::Ident("none".into()),
                Tok::Semi,
                Tok::Eof,
            ]
        );

        // Check that we have two files in the source map
        assert_eq!(sm.files.len(), 2);
        assert_eq!(sm.files[0].0.to_string_lossy(), "/etc/varnish/root.vcl");
        assert_eq!(sm.files[1].0.to_string_lossy(), "/etc/varnish/child.vcl");

        // Child's token should be in file 1
        let backend_token = &tokens[0];
        assert_eq!(backend_token.span.file, 1);
    }

    #[test]
    fn v3_branching_includes_dfs_order() {
        // Root includes child A, which includes grandchild B, then root includes sibling C
        // Verify chunk consumption order == index order and splicing is correct
        let root = "include \"a.vcl\"; include \"c.vcl\";";
        let a = "include \"b.vcl\";";
        let b = "backend b none;";
        let c = "backend c none;";

        let chunks = vec![
            crate::vclshow::VclShowChunk {
                index: 0,
                declared_len: root.len() as u32,
                filename: "/etc/varnish/root.vcl".to_string(),
                content: root.to_string(),
            },
            crate::vclshow::VclShowChunk {
                index: 1,
                declared_len: a.len() as u32,
                filename: "/etc/varnish/a.vcl".to_string(),
                content: a.to_string(),
            },
            crate::vclshow::VclShowChunk {
                index: 2,
                declared_len: b.len() as u32,
                filename: "/etc/varnish/b.vcl".to_string(),
                content: b.to_string(),
            },
            crate::vclshow::VclShowChunk {
                index: 3,
                declared_len: c.len() as u32,
                filename: "/etc/varnish/c.vcl".to_string(),
                content: c.to_string(),
            },
        ];

        let (tokens, sm) = lex_from_vcl_show(&chunks).expect("lex ok");
        let toks: Vec<Tok> = tokens.iter().map(|t| t.tok.clone()).collect();

        // All chunks consumed in order: root -> a -> b, then c
        // Content order: b, c (root's includes are replaced inline in DFS order)
        assert_eq!(
            toks,
            vec![
                Tok::Ident("backend".into()),
                Tok::Ident("b".into()),
                Tok::Ident("none".into()),
                Tok::Semi,
                Tok::Ident("backend".into()),
                Tok::Ident("c".into()),
                Tok::Ident("none".into()),
                Tok::Semi,
                Tok::Eof,
            ]
        );

        assert_eq!(sm.files.len(), 4);
    }

    #[test]
    fn v4_trailing_builtin_chunk_dropped() {
        // Trailing "Builtin" chunk after tree fully consumed — no error, silently dropped
        let root = "backend default none;";
        let builtin = "sub vcl_init { return (ok); }";

        let chunks = vec![
            crate::vclshow::VclShowChunk {
                index: 0,
                declared_len: root.len() as u32,
                filename: "/etc/varnish/root.vcl".to_string(),
                content: root.to_string(),
            },
            crate::vclshow::VclShowChunk {
                index: 1,
                declared_len: builtin.len() as u32,
                filename: "Builtin".to_string(),
                content: builtin.to_string(),
            },
        ];

        let (tokens, sm) = lex_from_vcl_show(&chunks).expect("lex ok");

        // Should parse without error; Builtin chunk is never added to source map (not consumed)
        // Tokens only from root
        assert!(matches!(tokens.last(), Some(t) if matches!(t.tok, Tok::Eof)));

        // Builtin chunk should NOT be in source map (it's not consumed by the tree)
        // Only root should be there
        assert_eq!(sm.files.len(), 1);
        assert_eq!(sm.files[0].0.to_string_lossy(), "/etc/varnish/root.vcl");
    }

    #[test]
    fn v5_trailing_non_builtin_leftover_errors() {
        // Trailing chunk with any other name — error naming the leftover filename
        let root = "backend default none;";
        let unused = "backend unused none;";

        let chunks = vec![
            crate::vclshow::VclShowChunk {
                index: 0,
                declared_len: root.len() as u32,
                filename: "/etc/varnish/root.vcl".to_string(),
                content: root.to_string(),
            },
            crate::vclshow::VclShowChunk {
                index: 1,
                declared_len: unused.len() as u32,
                filename: "/etc/varnish/unused.vcl".to_string(),
                content: unused.to_string(),
            },
        ];

        let err = lex_from_vcl_show(&chunks).expect_err("expected error");
        assert!(err.msg.contains("unused"), "msg: {}", err.msg);
        assert!(
            err.msg.contains("/etc/varnish/unused.vcl"),
            "msg: {}",
            err.msg
        );
    }

    #[test]
    fn v6_include_with_no_chunks_left_errors() {
        // include statement but no chunks left (state.next >= chunks.len()) — error
        let root = "include \"child.vcl\";";

        let chunks = vec![crate::vclshow::VclShowChunk {
            index: 0,
            declared_len: root.len() as u32,
            filename: "/etc/varnish/root.vcl".to_string(),
            content: root.to_string(),
        }];

        let err = lex_from_vcl_show(&chunks).expect_err("expected error");
        assert!(err.msg.contains("include"), "msg: {}", err.msg);
        assert!(err.msg.contains("chunk"), "msg: {}", err.msg);
    }

    #[test]
    fn v7_empty_chunks_slice_errors() {
        // Empty chunks slice — error ("no VCL.SHOW chunks")
        let chunks = vec![];
        let err = lex_from_vcl_show(&chunks).expect_err("expected error");
        assert!(err.msg.contains("no VCL.SHOW chunks"), "msg: {}", err.msg);
    }
}
