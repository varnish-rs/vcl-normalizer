#!/usr/bin/env python3
"""mutate.py -- VCL corpus mutator (test tooling only).

Applies a set of textual, best-effort mutations to a VCL 4.1 source file.
Used by the vcl-normalizer integration test corpus to produce pairs of files that
are (a) expected to be functionally *equivalent* after normalization
(formatting/renaming/reordering-only mutations) or (b) expected to be
*inequivalent* (`--break` semantic mutations).

This tool never uses the vcl-normalizer lexer/parser -- it is deliberately a
separate, dumber implementation, so that the two tools don't share blind
spots. It works on the raw text using a small brace/string/comment aware
scanner (see `iter_spans`) that is *just* smart enough to:

  - find top-level declaration boundaries (for --shuffle / --split-includes)
  - know current brace nesting depth per line (for --reindent)
  - know which byte ranges are inside comments / strings / inline-C, so
    those mutations never corrupt string or comment content.

All operations are deterministic given the same `--seed`.
"""

import argparse
import pathlib
import random
import re
import shutil
import subprocess
import sys

# ---------------------------------------------------------------------------
# Low-level text scanner: comments, strings, inline-C, brace-aware.
# ---------------------------------------------------------------------------


def _match_special(text, i):
    """If a comment/string/inline-C construct starts at text[i], return
    (end_index, kind); otherwise return None. `end_index` is exclusive."""
    n = len(text)
    c = text[i]
    if c == "#":
        j = text.find("\n", i)
        return (n if j == -1 else j + 1), "comment"
    if text[i : i + 2] == "//":
        j = text.find("\n", i)
        return (n if j == -1 else j + 1), "comment"
    if text[i : i + 2] == "/*":
        j = text.find("*/", i + 2)
        return (n if j == -1 else j + 2), "comment"
    if text[i : i + 2] == "C{":
        j = text.find("}C", i + 2)
        return (n if j == -1 else j + 2), "csource"
    if text[i : i + 3] == '"""':
        j = text.find('"""', i + 3)
        return (n if j == -1 else j + 3), "tstring"
    if text[i : i + 2] == '{"':
        j = text.find('"}', i + 2)
        return (n if j == -1 else j + 2), "cstring"
    if c == '"':
        j = text.find('"', i + 1)
        return (n if j == -1 else j + 1), "dstring"
    return None


def iter_spans(text):
    """Yield (start, end, kind) spans covering the whole text contiguously.
    kind is one of: code, comment, csource, tstring, cstring, dstring."""
    n = len(text)
    i = 0
    while i < n:
        m = _match_special(text, i)
        if m is not None:
            end, kind = m
            yield (i, end, kind)
            i = end
            continue
        start = i
        i += 1
        while i < n and _match_special(text, i) is None:
            i += 1
        yield (start, i, "code")


def compute_positions_depth(text):
    """Per-character arrays: depth_at[i] = brace nesting depth "at" i
    (post-decrement for a closing brace, pre-increment for an opening one,
    so the depth recorded on a '{' or '}' char is the *outer* level of the
    line it starts), kind_at[i] = span kind covering i."""
    n = len(text)
    depth_at = [0] * (n + 1)
    kind_at = ["code"] * (n + 1)
    depth = 0
    for s, e, kind in iter_spans(text):
        for idx in range(s, e):
            kind_at[idx] = kind
            if kind == "code":
                c = text[idx]
                if c == "{":
                    depth_at[idx] = depth
                    depth += 1
                elif c == "}":
                    depth -= 1
                    depth_at[idx] = depth
                else:
                    depth_at[idx] = depth
            else:
                depth_at[idx] = depth
    depth_at[n] = depth
    return depth_at, kind_at


def find_matching_paren(text, open_idx):
    """Given text[open_idx] == '(', return the index of the matching ')'."""
    n = len(text)
    depth = 0
    j = open_idx
    while j < n:
        m = _match_special(text, j)
        if m is not None:
            j = m[0]
            continue
        c = text[j]
        if c == "(":
            depth += 1
            j += 1
            continue
        if c == ")":
            depth -= 1
            if depth == 0:
                return j
            j += 1
            continue
        j += 1
    return None


def split_top_commas(s):
    """Split an argument-list string on top-level commas (parens/strings
    aware)."""
    parts = []
    cur = []
    depth = 0
    i = 0
    n = len(s)
    while i < n:
        m = _match_special(s, i)
        if m is not None:
            end, _kind = m
            cur.append(s[i:end])
            i = end
            continue
        c = s[i]
        if c == "(":
            depth += 1
            cur.append(c)
            i += 1
            continue
        if c == ")":
            depth -= 1
            cur.append(c)
            i += 1
            continue
        if c == "," and depth == 0:
            parts.append("".join(cur))
            cur = []
            i += 1
            continue
        cur.append(c)
        i += 1
    if cur:
        parts.append("".join(cur))
    return [p.strip() for p in parts if p.strip() != ""]


# ---------------------------------------------------------------------------
# Top-level declaration chunking (for --shuffle / --split-includes)
# ---------------------------------------------------------------------------


def _skip_ws(text, i):
    n = len(text)
    while i < n:
        m = _match_special(text, i)
        if m is not None:
            i = m[0]
            continue
        if text[i] in " \t\r\n":
            i += 1
            continue
        break
    return i


def split_top_level(text):
    """Return (head, chunks): `head` is the leading `vcl X.Y;` declaration
    (with any leading comments dropped), `chunks` is the list of raw source
    texts for every subsequent top-level declaration (import/include/
    backend/probe/acl/sub), in file order."""
    n = len(text)
    i = _skip_ws(text, 0)
    head_start = i
    while i < n:
        m = _match_special(text, i)
        if m is not None:
            i = m[0]
            continue
        if text[i] == ";":
            i += 1
            break
        i += 1
    head_end = i
    head = text[head_start:head_end]

    chunks = []
    i = _skip_ws(text, i)
    while i < n:
        start = i
        depth = 0
        while i < n:
            m = _match_special(text, i)
            if m is not None:
                i = m[0]
                continue
            c = text[i]
            if c == "{":
                depth += 1
                i += 1
                continue
            if c == "}":
                depth -= 1
                i += 1
                if depth == 0:
                    break
                continue
            if c == ";" and depth == 0:
                i += 1
                break
            i += 1
        chunks.append(text[start:i])
        i = _skip_ws(text, i)
    return head, chunks


def join_program(head, chunks):
    parts = [head] + list(chunks)
    return "\n\n".join(parts) + "\n"


# ---------------------------------------------------------------------------
# Operations
# ---------------------------------------------------------------------------

DECL_NAME_RE = re.compile(
    r"^(backend|probe|acl|sub)\s+([A-Za-z_][A-Za-z0-9_-]*)", re.MULTILINE
)

NAME_POOL = [
    "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel",
    "india", "juliet", "kilo", "lima", "mike", "november", "oscar", "papa",
    "quebec", "romeo", "sierra", "tango", "uniform", "victor", "whiskey",
    "xray", "yankee", "zulu",
]


def op_rename(text, rng):
    """Word-boundary regex rename of declared backend/probe/acl/sub names.
    Skips builtin `vcl_*` subs and the implicit `default` backend."""
    names = []
    seen = set()
    for m in DECL_NAME_RE.finditer(text):
        name = m.group(2)
        if name.startswith("vcl_") or name == "default":
            continue
        if name not in seen:
            seen.add(name)
            names.append(name)
    if not names:
        return text

    pool = NAME_POOL[:]
    rng.shuffle(pool)
    mapping = {}
    for i, name in enumerate(names):
        base = pool[i % len(pool)]
        mapping[name] = f"m_{base}_{i}"

    keys_sorted = sorted(mapping.keys(), key=len, reverse=True)
    pattern = re.compile(r"\b(" + "|".join(re.escape(k) for k in keys_sorted) + r")\b")
    return pattern.sub(lambda m: mapping[m.group(1)], text)


def op_shuffle(text, rng):
    """Permute top-level declarations (backend/probe/acl/sub/import/include),
    keeping the leading `vcl X.Y;` declaration first."""
    head, chunks = split_top_level(text)
    chunks = chunks[:]
    rng.shuffle(chunks)
    return join_program(head, chunks)


def op_split_includes(text, n, rng, part_filename):
    """Move `n` top-level declarations into a new file, replacing them with
    a single `include "part_filename";` at the position of the first moved
    chunk. Returns (main_text, part_text)."""
    head, chunks = split_top_level(text)
    n = min(n, len(chunks))
    if n <= 0:
        return text, None
    idxs = sorted(rng.sample(range(len(chunks)), n))
    moved = [chunks[i] for i in idxs]
    idxset = set(idxs)
    remaining = [c for i, c in enumerate(chunks) if i not in idxset]
    part_text = "\n\n".join(moved) + "\n"
    include_stmt = f'include "{part_filename}";'
    remaining.insert(idxs[0], include_stmt)
    main_text = join_program(head, remaining)
    return main_text, part_text


def op_reindent(text, rng):
    """Recompute leading whitespace of every non-continuation line from
    actual brace nesting depth, using a randomly chosen indent unit (tab or
    N spaces). Lines that are continuations of a multi-line string/comment
    are left untouched so their content is never corrupted."""
    depth_at, kind_at = compute_positions_depth(text)
    unit = rng.choice(["\t", "  ", "   ", "    ", "        "])
    n = len(text)
    lines = text.split("\n")
    out_lines = []
    offset = 0
    for line in lines:
        line_start = offset
        offset += len(line) + 1
        stripped = line.lstrip(" \t")
        if stripped == "":
            out_lines.append("")
            continue
        lead_len = len(line) - len(stripped)
        content_start = line_start + lead_len
        if content_start >= n or kind_at[content_start] != "code":
            out_lines.append(line)
            continue
        depth = max(0, depth_at[content_start])
        out_lines.append(unit * depth + stripped)
    return "\n".join(out_lines)


COMMENT_STYLES = [
    lambda s: f"# {s}",
    lambda s: f"// {s}",
    lambda s: f"/* {s} */",
]

COMMENT_NOTES = [
    "mutation marker", "generated by mutate.py", "note", "checkpoint",
    "informational", "see ticket", "reviewed",
]


def op_comments(text, rng):
    """Inject comment lines (one of the three VCL comment styles) at line
    boundaries, never inside a continuing multi-line string/comment."""
    _depth_at, kind_at = compute_positions_depth(text)
    n = len(text)
    lines = text.split("\n")
    offsets = []
    offset = 0
    for line in lines:
        offsets.append(offset)
        offset += len(line) + 1

    out_lines = []
    for idx, line in enumerate(lines):
        line_start = offsets[idx]
        boundary_ok = line_start >= n or kind_at[line_start] == "code"
        if boundary_ok and line.strip() != "" and rng.random() < 0.3:
            style = rng.choice(COMMENT_STYLES)
            out_lines.append(style(rng.choice(COMMENT_NOTES)))
        out_lines.append(line)
    return "\n".join(out_lines)


def op_string_style(text, rng):
    """Rewrite eligible single-line "..." strings to the {"..."} long-string
    form (each independently, at 50% probability)."""
    out = []
    for s, e, kind in iter_spans(text):
        seg = text[s:e]
        if kind == "dstring" and rng.random() < 0.5:
            inner = seg[1:-1]
            seg = '{"' + inner + '"}'
        out.append(seg)
    return "".join(out)


ARG_TABLES = {
    "std.duration": ["s", "fallback", "real", "integer"],
    "std.bytes": ["s", "fallback", "real", "integer"],
    "std.integer": ["s", "fallback", "boolean", "bytes", "duration", "real", "time"],
}

NAMED_ARG_RE = re.compile(r"^([A-Za-z_][A-Za-z0-9_-]*)\s*=\s*(?!=)(.*)$", re.DOTALL)


def op_arg_style(text, rng):
    """Flip positional<->named argument style for a small hardcoded table of
    known `std.*` calls. Best-effort: leaves anything it isn't sure about
    unchanged."""
    call_re = re.compile(r"\b(" + "|".join(re.escape(k) for k in ARG_TABLES) + r")\s*\(")
    out = []
    i = 0
    n = len(text)
    while i < n:
        m = _match_special(text, i)
        if m is not None:
            out.append(text[i : m[0]])
            i = m[0]
            continue
        mm = call_re.match(text, i)
        if mm:
            fname = mm.group(1)
            paren_start = mm.end() - 1
            close = find_matching_paren(text, paren_start)
            if close is not None:
                args_str = text[paren_start + 1 : close]
                args = split_top_commas(args_str)
                slots = ARG_TABLES[fname]
                named_matches = [NAMED_ARG_RE.match(a) for a in args]
                new_call = None
                if args and all(named_matches):
                    slot_names = [mo.group(1) for mo in named_matches]
                    if slot_names == slots[: len(slot_names)]:
                        new_args = ", ".join(mo.group(2).strip() for mo in named_matches)
                        new_call = f"{fname}({new_args})"
                elif args and not any(named_matches):
                    if len(args) <= len(slots):
                        new_args = ", ".join(
                            f"{slots[k]} = {args[k]}" for k in range(len(args))
                        )
                        new_call = f"{fname}({new_args})"
                if new_call is not None:
                    out.append(new_call)
                    i = close + 1
                    continue
                out.append(text[mm.start() : close + 1])
                i = close + 1
                continue
        out.append(text[i])
        i += 1
    return "".join(out)


DURATION_RE = re.compile(r"(?<![\w.])(\d+(?:\.\d+)?)(ms|s|m|h|d|w|y)\b")


def op_break_ttl(text, _rng):
    """Semantic mutation: change the first duration literal found."""
    for s, e, kind in iter_spans(text):
        if kind != "code":
            continue
        seg = text[s:e]
        m = DURATION_RE.search(seg)
        if m:
            num = float(m.group(1)) + 1
            num_str = str(int(num)) if num == int(num) else str(num)
            new_seg = seg[: m.start()] + num_str + m.group(2) + seg[m.end() :]
            return text[:s] + new_seg + text[e:]
    return text


def op_break_cond(text, _rng):
    """Semantic mutation: negate the first `if (...)` condition."""
    for s, e, kind in iter_spans(text):
        if kind != "code":
            continue
        seg = text[s:e]
        mm = re.search(r"\bif\s*\(", seg)
        if mm:
            open_idx = s + mm.end() - 1
            close = find_matching_paren(text, open_idx)
            if close is not None:
                return (
                    text[: open_idx + 1]
                    + "!("
                    + text[open_idx + 1 : close]
                    + ")"
                    + text[close:]
                )
    return text


def op_break_drop_set(text, _rng):
    """Semantic mutation: delete the first `set ...;` statement line."""
    lines = text.split("\n")
    for idx, line in enumerate(lines):
        if re.match(r"^\s*set\s+\S.*;\s*$", line):
            del lines[idx]
            return "\n".join(lines)
    return text


BREAK_OPS = {
    "ttl": op_break_ttl,
    "cond": op_break_cond,
    "drop-set": op_break_drop_set,
}


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def build_arg_parser():
    ap = argparse.ArgumentParser(
        prog="mutate.py", description="VCL corpus mutator (test tooling only)"
    )
    ap.add_argument("input", help="input VCL file")
    ap.add_argument(
        "-o",
        "--output",
        help="output file path; output DIRECTORY when --split-includes is used "
        "(default: print to stdout)",
    )
    ap.add_argument("--seed", type=int, default=0, help="PRNG seed (deterministic)")
    ap.add_argument("--reindent", action="store_true", help="tabs<->spaces, random indent width")
    ap.add_argument("--comments", action="store_true", help="inject comments at line boundaries")
    ap.add_argument("--rename", action="store_true", help="rename declared backend/probe/acl/sub")
    ap.add_argument("--shuffle", action="store_true", help="permute top-level declarations")
    ap.add_argument(
        "--split-includes",
        type=int,
        default=0,
        metavar="N",
        help="move N top-level declarations into a new included file",
    )
    ap.add_argument(
        "--string-style", action="store_true", help='rewrite "..." to {"..."} where eligible'
    )
    ap.add_argument(
        "--arg-style", action="store_true", help="flip positional<->named args for known std calls"
    )
    ap.add_argument(
        "--break",
        dest="break_kind",
        choices=["ttl", "cond", "drop-set"],
        default=None,
        help="semantic mutation for expected-unequal pairs",
    )
    ap.add_argument(
        "--verify",
        action="store_true",
        help="run `varnishd -C` on the produced output when varnishd is available",
    )
    return ap


def run_verify(main_path: pathlib.Path, input_path: pathlib.Path):
    if shutil.which("varnishd") is None:
        print("note: varnishd not found on PATH; skipping --verify", file=sys.stderr)
        return 0
    vcl_path_dirs = [str(main_path.parent.resolve()), str(input_path.resolve().parent)]
    cmd = [
        "varnishd",
        "-C",
        "-p",
        f"vcl_path={':'.join(vcl_path_dirs)}",
        "-f",
        str(main_path.resolve()),
    ]
    proc = subprocess.run(cmd, capture_output=True, text=True)
    if proc.returncode != 0:
        print(f"--verify FAILED (varnishd -C exit {proc.returncode}):", file=sys.stderr)
        print(proc.stderr, file=sys.stderr)
        return 1
    print("--verify OK (varnishd -C accepted the output)", file=sys.stderr)
    return 0


def main(argv=None):
    ap = build_arg_parser()
    args = ap.parse_args(argv)

    input_path = pathlib.Path(args.input)
    try:
        text = input_path.read_text()
    except OSError as e:
        print(f"error: cannot read {args.input}: {e}", file=sys.stderr)
        return 3

    rng = random.Random(args.seed)

    if args.break_kind is not None:
        text = BREAK_OPS[args.break_kind](text, rng)

    if args.rename:
        text = op_rename(text, rng)
    if args.arg_style:
        text = op_arg_style(text, rng)
    if args.string_style:
        text = op_string_style(text, rng)
    if args.shuffle:
        text = op_shuffle(text, rng)

    part_text = None
    part_filename = None
    if args.split_includes and args.split_includes > 0:
        stem = input_path.stem
        part_filename = f"{stem}_split1.vcl"
        text, part_text = op_split_includes(text, args.split_includes, rng, part_filename)

    if args.reindent:
        text = op_reindent(text, rng)
    if args.comments:
        text = op_comments(text, rng)

    result_main = None
    if part_text is not None:
        if not args.output:
            print("error: --split-includes requires -o <directory>", file=sys.stderr)
            return 3
        outdir = pathlib.Path(args.output)
        outdir.mkdir(parents=True, exist_ok=True)
        main_path = outdir / input_path.name
        part_path = outdir / part_filename
        main_path.write_text(text)
        part_path.write_text(part_text)
        result_main = main_path
    else:
        if args.output:
            out_path = pathlib.Path(args.output)
            if out_path.parent != pathlib.Path(""):
                out_path.parent.mkdir(parents=True, exist_ok=True)
            out_path.write_text(text)
            result_main = out_path
        else:
            sys.stdout.write(text)

    if args.verify:
        if result_main is None:
            print("warning: --verify needs -o to know what file to check; skipping", file=sys.stderr)
        else:
            rc = run_verify(result_main, input_path)
            if rc != 0:
                return 1

    return 0


if __name__ == "__main__":
    sys.exit(main())
