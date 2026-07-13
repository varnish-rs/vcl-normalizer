# AGENTS.md

Instructions for AI coding agents working in this repo. Humans: see `README.md` for user-facing docs.

## What this is

`vcl-normalizer` — a Rust CLI that compares two VCL 4.1 files for functional equivalence (see `README.md` for what "equivalent" means and doesn't mean). Single binary crate, no workspace.

## Pipeline / architecture

```
VCL files ──► lexer (+include splicing) ──► parser ──► AST ──► validate ──► normalize ──► canonical AST
                                                                                              │
                                              two canonical ASTs ──► compare ──► exit code + report / diff
```

| Stage | File |
|---|---|
| CLI (clap) | `src/main.rs` |
| AST types, `Span`, `SourceMap` | `src/ast.rs` |
| Lexer (+ `include` splicing) | `src/lexer.rs` |
| Recursive-descent parser | `src/parser.rs` |
| Symbol-table validation | `src/symbols.rs` |
| Vmod `.so` spec extraction | `src/vmod.rs` |
| Normalize passes (fixed order, see `src/normalize/mod.rs`) | `src/normalize/{literals,vmod_args,probes,rename,sort}.rs` |
| Canonical JSON output | `src/canon.rs` |
| Canonical VCL pretty-printer | `src/printer.rs` |
| Tree diff / divergence report | `src/compare.rs` |
| Corpus mutator (test tooling only) | `tools/mutate.py` |

### `src/ast.rs` is the frozen contract

Every other module codes against these types. If a change requires touching `ast.rs`, stop and think — it affects every pass. Prefer extending (a new field with a sensible default, a new enum variant) over restructuring existing shapes.

## `varnishd -C` is the ground truth

This tool is deliberately more lenient than real VCC in some places (see README's "What it does not mean" section) — that's by design, not oversight. But wherever `vcl-normalizer` claims something is valid/invalid VCL, or how a construct behaves, **verify against real `varnishd -C` first** if there's any doubt — don't guess from memory. This project has repeatedly found real behavior (multiple bugs) that differed from initial assumptions:
- Same-name `sub` redeclaration is legal *only* for the 14 real builtin `vcl_*` hooks (`ast::BUILTIN_SUB_ORDER`) — a custom name redeclared, or a `vcl_`-prefixed typo of a builtin, is a hard compile error ("the names 'vcl_*' are reserved for subroutines").
- Inline `.probe = { ... }` blocks take **no** trailing `;` after the closing `}` — unlike every other field-value form.
- Writing a header (`set req.http.X = ...`) is case-*sensitive* (determines the literal name on the wire); reading one (`req.http.X` in an expression) is a case-*insensitive* lookup. Don't normalize both the same way.

If you're unsure whether something is real Varnish behavior, `varnishd` and `varnishd -C -f <file>` are available in dev/CI — use them rather than asserting from memory. Note: this sandbox's `varnishd` build ("The Vinyl Cache Project") accepts ~28 extra internal `vcl_*` hook names beyond real/standard Varnish — don't take everything this specific binary accepts as ground truth for what a real customer's Varnish would accept; cross-check against the Varnish Book / `vcl(7)` when something looks exotic.

## Testing conventions

- **Every bug fix gets both a unit test and a CLI-level integration test** (`tests/integration.rs`, driving the built binary), even when the unit test already covers the logic. Do this proactively, don't wait to be asked.
- Normalize passes are pure `AST → AST` functions, unit-tested on hand-built ASTs via `ast::builder` (`#[cfg(any(test, feature = "testutil"))]` in `ast.rs`) — no need to go through the parser for pass-level tests.
- `tests/corpus/seed*.vcl` are hand-written fixtures, each verified to compile under real `varnishd -C` (see below). `tools/mutate.py` derives equivalent/inequivalent mutations from them for `tests/integration.rs`'s corpus-matrix test. Adding a new seed: verify it with `varnishd -C -f <path>` before relying on it in a test (multi-file seeds need `-p vcl_path=<dir>` for includes to resolve).

## Commands

```sh
cargo build                              # dev build
cargo test                                # unit + integration tests
cargo fmt --check                         # formatting gate (CI)
cargo clippy --all-targets -- -D warnings # lint gate (CI) — must be clean, no drive-by allows without a comment
```

CI (`.github/workflows/ci.yml`) runs all four on every push/PR. Keep the crate `cargo clippy --all-targets -- -D warnings` clean — if you must allow a lint, do it locally on the specific item with a one-line rationale comment (see `src/compare.rs`'s `#[allow(clippy::too_many_arguments)]` for the pattern), not crate-wide.

There is no `varnishd` in CI, and `cargo test` never shells out to it directly — corpus fixtures are verified against real `varnishd -C` manually during development (or via `tools/mutate.py --verify`, which itself skips gracefully if `varnishd` isn't on `PATH`). The one test that does need an external tool at runtime, `vmod::tests::v6_system_vmod_if_available`, auto-skips via a `pkg-config` check; `tests/integration.rs`'s `i2_corpus_matrix` skips if `python3`/`tools/mutate.py` aren't usable (see `mutate_available()`). Follow that same skip pattern — `eprintln!` + early return, never a hard failure — for any new test that depends on an optional external tool.
