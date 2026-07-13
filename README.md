# vcl-normalizer

`vcl-normalizer` compares two VCL 4.1 files for **functional equivalence**: it parses
both, normalizes each to a canonical form (stable naming, sorted
declarations, canonicalized literals/vmod arguments, lifted inline probes),
and reports whether the two canonical forms match.

It exists because programmatically generated VCL rarely matches a
hand-written reference byte-for-byte — comments, indentation, object names,
declaration order, and file layout (`include`s) legitimately differ without
changing behavior. Exact textual diff is too strict for that; `vcl-normalizer`
answers the more useful question.

## Usage

```
vcl-normalizer dump    [OPTIONS] <FILE>              # canonical JSON to stdout
vcl-normalizer print   [OPTIONS] <FILE>              # canonical VCL (pretty-printed) to stdout
vcl-normalizer compare [OPTIONS] <FILE_A> <FILE_B>   # equivalence check
```

### Common options (all subcommands)

| Flag | Meaning |
|---|---|
| `-I <DIR>` | Include search path, repeatable; searched (in order) after the including file's own directory. |
| `--vmod-path <DIR>` | Vmod `.so` search path, repeatable. Default: the output of `pkg-config --variable=vmoddir varnishapi`, if available. |
| `--no-vmod` | Skip vmod spec loading entirely; vmod calls are then handled structurally only (no signature/argument checks or canonicalization). |
| `--from-vcl-show` | Parse `<FILE>` as a `varnishadm vcl.show -v` dump (e.g. from `varnishgather`) instead of a plain VCL file. Every `include` resolves against the other files already present in the dump, in document order; the real filesystem and `-I`/`--vcl-path` are never consulted (a warning is printed if `-I`/`--vcl-path` is also given). |

Vmods that can't be located or whose `.so` can't be parsed are silently
skipped (with a warning on stderr) rather than treated as an error — calls
into them are then compared structurally, verbatim.

### `print`-only options

| Flag | Meaning |
|---|---|
| `--rename` | Apply canonical renaming (`backend_1`, `sub_2`, ...) — same as `dump`/`compare` always do. Off by default: original backend/probe/acl/sub names are clearer to read. |

### `compare`-only options

| Flag | Meaning |
|---|---|
| `--diff` | Also print a unified diff (3 lines of context) of the two canonical pretty-prints. |
| `--max-reports <N>` | Cap on the number of reported divergences (default 20). |
| `--names` | Print the discovered name bijection (canonical name vs. original spelling) for both files, side by side. |

### Examples

```sh
vcl-normalizer compare generated.vcl reference.vcl
vcl-normalizer compare --diff --names -I ./vcl_lib generated.vcl reference.vcl
vcl-normalizer dump --no-vmod some.vcl | jq .
```

## Exit codes

| Code | Meaning |
|---|---|
| 0 | The two files are equivalent (or, for `dump`/`print`, the file parsed/validated/normalized successfully). |
| 1 | The two files are **not** equivalent; a divergence report (and optionally a diff / name table) is printed to stdout. |
| 2 | Parse error or validation (symbol-resolution) error. A `file:line:col` diagnostic with a caret is printed to stderr. |
| 3 | Usage/IO error: missing file, bad CLI flags, etc. |

## What "equivalent" means (and does not mean)

Two files are equivalent if, after normalization, their canonical ASTs are
identical. Normalization accounts for:

- **Naming**: backends, probes, ACLs, and custom subs are renamed to a
  canonical scheme based on structural role and order of first use, so
  `web01`/`web02` in one file and `b1`/`b2` in the other compare equal.
- **Formatting**: whitespace, indentation style, and comments never affect
  comparison (`vcl-normalizer` compares parsed structure, not text).
- **Top-level declaration order**: declarations are sorted into a fixed,
  deterministic order before comparison.
- **Literal spelling**: numbers, durations (`1m` vs `60s`), byte sizes, and
  header-name case are canonicalized.
- **Vmod call argument style**: named vs. positional arguments (and
  explicit-default vs. omitted optional arguments) are canonicalized when
  the vmod's spec is available.
- **Inline vs. named probes**: an inline `.probe = { ... }` is lifted to an
  equivalent top-level probe declaration before comparison.
- **File layout**: `include`d files are spliced in at lex time, so the
  same logical program compares equal regardless of how it's split across
  files.

What it explicitly does **not** claim:

- **No expression- or condition-reordering equivalence.** `if (a && b)` is
  *not* considered equivalent to `if (b && a)`, and reordering independent
  `set` statements is a real divergence, even though both may be
  behaviorally identical in practice. Equivalence here is *structural*
  equivalence modulo naming/formatting/declaration-order — not full
  semantic/logical equivalence.
- **No type checking or return-action legality checking.** `vcl-normalizer`'s
  validation pass only catches name-resolution mistakes (undefined
  backends/ACLs/probes/subs, bad vmod arity/named-argument names) that
  would otherwise silently confuse normalization; it does not reimplement
  VCC's full semantic checks (e.g. which `return()` actions are legal in
  which built-in sub).
- **No validation of backend/probe field names.** Any `.ident = value;`
  parses; field names are not checked against a hard-coded table, since
  they vary across Varnish point releases.

**`varnishd -C` remains the authoritative VCL compiler** for questions of
syntactic and semantic *validity*. `vcl-normalizer` is deliberately more lenient
than VCC in places (see above) so that it can focus on the orthogonal
question of *equivalence* between two files that are each assumed to be
independently valid (or at least intended to be).
