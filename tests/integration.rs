//! CLI / end-to-end integration tests (spec §"CLI / integration", I1-I5).
//!
//! Drives the built `vcl-normalizer` binary via `std::process::Command`. All
//! commands run with the crate root as the working directory (regardless
//! of whatever cwd `cargo test` happens to use), and all corpus/fixture
//! paths are resolved from `CARGO_MANIFEST_DIR` so the suite is
//! location-independent.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn corpus(name: &str) -> PathBuf {
    manifest_dir().join("tests/corpus").join(name)
}

/// Runs the built `vcl-normalizer` binary with the given args, cwd = crate root.
fn run_bin(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_vcl-normalizer"))
        .args(args)
        .current_dir(manifest_dir())
        .output()
        .expect("failed to run vcl-normalizer")
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A fresh, empty temp directory, unique per call (safe under `cargo test`'s
/// default parallel test execution).
fn unique_tmp_dir(tag: &str) -> PathBuf {
    let id = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "vcl-normalizer-integration-{}-{}-{}",
        std::process::id(),
        tag,
        id
    ));
    fs::create_dir_all(&dir).expect("failed to create temp dir");
    dir
}

// ─────────────────────────── I1 ───────────────────────────

/// Exit codes: 0 equal, 1 differ, 2 on parse error, 2 on validation error,
/// 3 on missing file/bad flag.
#[test]
fn i1_exit_codes() {
    // 0: trivially equal pair (same file both sides).
    let out = run_bin(&[
        "compare",
        corpus("seed1.vcl").to_str().unwrap(),
        corpus("seed1.vcl").to_str().unwrap(),
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "equal pair should exit 0\nstdout:{}\nstderr:{}",
        stdout_of(&out),
        stderr_of(&out)
    );
    assert!(stdout_of(&out).contains("equivalent"));

    // 1: trivially different pair.
    let out = run_bin(&[
        "compare",
        corpus("seed1.vcl").to_str().unwrap(),
        corpus("seed2.vcl").to_str().unwrap(),
    ]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "different pair should exit 1\nstdout:{}\nstderr:{}",
        stdout_of(&out),
        stderr_of(&out)
    );

    // 2: syntax-error file (missing semicolon).
    let tmp = unique_tmp_dir("i1-syntax");
    let bad = tmp.join("bad.vcl");
    fs::write(
        &bad,
        "vcl 4.1;\nbackend b1 {\n    .host = \"127.0.0.1\"\n}\n",
    )
    .unwrap();
    let out = run_bin(&["dump", bad.to_str().unwrap()]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "syntax error should exit 2\nstdout:{}\nstderr:{}",
        stdout_of(&out),
        stderr_of(&out)
    );
    assert!(
        !stderr_of(&out).is_empty(),
        "syntax error should print a diagnostic to stderr"
    );

    // 2: validation error (undefined symbol reference).
    let tmp2 = unique_tmp_dir("i1-semantic");
    let bad2 = tmp2.join("bad_sem.vcl");
    fs::write(
        &bad2,
        "vcl 4.1;\nbackend b1 {\n    .host = \"127.0.0.1\";\n}\nsub vcl_recv {\n    call does_not_exist;\n}\n",
    )
    .unwrap();
    let out = run_bin(&["dump", bad2.to_str().unwrap()]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "validation error should exit 2\nstdout:{}\nstderr:{}",
        stdout_of(&out),
        stderr_of(&out)
    );

    // 3: missing file.
    let out = run_bin(&["dump", corpus("does_not_exist_xyz.vcl").to_str().unwrap()]);
    assert_eq!(
        out.status.code(),
        Some(3),
        "missing file should exit 3\nstdout:{}\nstderr:{}",
        stdout_of(&out),
        stderr_of(&out)
    );

    // 3: bad flag.
    let out = run_bin(&["--this-flag-does-not-exist"]);
    assert_eq!(
        out.status.code(),
        Some(3),
        "bad flag should exit 3\nstdout:{}\nstderr:{}",
        stdout_of(&out),
        stderr_of(&out)
    );
}

// ─────────────────────────── I2 ───────────────────────────

/// Checks whether `python3 tools/mutate.py --help` works in this
/// environment; if not (no python3 on PATH, etc.) I2 is skipped gracefully.
fn mutate_available() -> bool {
    match Command::new("python3")
        .arg("tools/mutate.py")
        .arg("--help")
        .current_dir(manifest_dir())
        .output()
    {
        Ok(out) => out.status.success(),
        Err(_) => false,
    }
}

/// Runs `tools/mutate.py` on `input`, writing the result to `out`. Returns
/// true iff the process exited 0.
fn run_mutate(input: &Path, extra_args: &[&str], out: &Path) -> bool {
    let status = Command::new("python3")
        .arg("tools/mutate.py")
        .arg(input)
        .args(extra_args)
        .arg("--seed")
        .arg("42")
        .arg("-o")
        .arg(out)
        .current_dir(manifest_dir())
        .status()
        .expect("failed to run tools/mutate.py");
    status.success()
}

const SEEDS: &[&str] = &[
    "seed1.vcl",
    "seed2.vcl",
    "seed3.vcl",
    "seed4.vcl",
    "seed5.vcl",
    "seed6.vcl",
    "seed7.vcl",
];

/// seed4.vcl `include`s two sibling files in tests/corpus/; when a mutated
/// copy lives elsewhere, pass -I tests/corpus so those includes still
/// resolve.
fn include_args_for(seed: &str) -> Vec<&'static str> {
    if seed == "seed4.vcl" {
        vec!["-I", "tests/corpus"]
    } else {
        vec![]
    }
}

/// Equal-mutation combos to try for a given seed. `--shuffle` permutes
/// *every* top-level declaration with no regard for semantics; that's fine
/// for most seeds, but not for ones with same-named `sub vcl_recv`
/// fragments (concatenated in declaration order by VCC, so genuinely
/// order-sensitive -- see seed6.vcl's and seed7.vcl's header comments):
/// shuffling those really does change behavior. That's a real blind spot of
/// the deliberately-dumb text mutator, not a vcl-normalizer bug, so those seeds are
/// exercised with `--rename` alone instead of `--rename --shuffle`.
fn equal_combos_for(seed: &str) -> Vec<&'static [&'static str]> {
    if seed == "seed6.vcl" || seed == "seed7.vcl" {
        vec![&["--reindent", "--comments"], &["--rename"]]
    } else {
        vec![&["--reindent", "--comments"], &["--rename", "--shuffle"]]
    }
}

/// Full corpus matrix: every seed x every equal-mutation combo -> 0; every
/// seed x every `--break` -> 1.
#[test]
fn i2_corpus_matrix() {
    if !mutate_available() {
        eprintln!("skipping i2_corpus_matrix: `python3 tools/mutate.py --help` failed");
        return;
    }

    let break_kinds: &[&str] = &["ttl", "cond", "drop-set"];

    for &seed in SEEDS {
        let seed_path = corpus(seed);
        let inc_args = include_args_for(seed);
        let equal_combos = equal_combos_for(seed);

        for (i, combo) in equal_combos.iter().enumerate() {
            let tmp = unique_tmp_dir(&format!("i2-eq-{seed}-{i}"));
            let out_path = tmp.join(seed);
            assert!(
                run_mutate(&seed_path, combo, &out_path),
                "mutate.py failed for {seed} combo {combo:?}"
            );

            let mut args: Vec<&str> = vec!["compare"];
            args.extend(inc_args.iter().copied());
            let seed_str = seed_path.to_str().unwrap().to_string();
            let out_str = out_path.to_str().unwrap().to_string();
            args.push(&seed_str);
            args.push(&out_str);

            let out = run_bin(&args);
            assert_eq!(
                out.status.code(),
                Some(0),
                "seed {seed} combo {combo:?} expected equivalent (exit 0)\nstdout:{}\nstderr:{}",
                stdout_of(&out),
                stderr_of(&out)
            );
        }

        for &kind in break_kinds {
            let tmp = unique_tmp_dir(&format!("i2-brk-{seed}-{kind}"));
            let out_path = tmp.join(seed);
            assert!(
                run_mutate(&seed_path, &["--break", kind], &out_path),
                "mutate.py failed for {seed} --break {kind}"
            );

            let orig_text = fs::read_to_string(&seed_path).unwrap();
            let mutated_text = fs::read_to_string(&out_path).unwrap();
            if orig_text == mutated_text {
                // The break op found nothing to mutate in this particular
                // seed (e.g. no bare duration literal for --break ttl); a
                // no-op mutation is trivially "equivalent", not a bug.
                eprintln!("note: --break {kind} was a no-op for {seed}; skipping");
                continue;
            }

            let mut args: Vec<&str> = vec!["compare"];
            args.extend(inc_args.iter().copied());
            let seed_str = seed_path.to_str().unwrap().to_string();
            let out_str = out_path.to_str().unwrap().to_string();
            args.push(&seed_str);
            args.push(&out_str);

            let out = run_bin(&args);
            assert_eq!(
                out.status.code(),
                Some(1),
                "seed {seed} --break {kind} expected a divergence (exit 1)\nstdout:{}\nstderr:{}",
                stdout_of(&out),
                stderr_of(&out)
            );
        }
    }
}

// ─────────────────────────── I3 ───────────────────────────

/// Hand-written pair (not mutator-derived): A uses an inline probe, a
/// named vmod arg, and a `1m` duration; B uses a named top-level probe,
/// a positional vmod arg, and an equivalent `60s` duration. Same backend
/// otherwise. Guards against the mutator and the comparator sharing
/// blind spots.
#[test]
fn i3_hand_written_pair() {
    let out = run_bin(&[
        "compare",
        corpus("pair_a.vcl").to_str().unwrap(),
        corpus("pair_b.vcl").to_str().unwrap(),
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "hand-written equivalent pair should exit 0\nstdout:{}\nstderr:{}",
        stdout_of(&out),
        stderr_of(&out)
    );
    assert!(stdout_of(&out).contains("equivalent"));
}

// ─────────────────────────── I4 ───────────────────────────

/// `--diff` output contains the changed line and nothing outside the
/// 3-line context; `--names` prints the bijection.
#[test]
fn i4_diff_and_names() {
    // seed2.vcl has real renameable declarations (probes/acls/subs), so
    // --names has non-trivial content; it's also long enough that a
    // localized one-line change lets us check the diff's context radius.
    let seed_path = corpus("seed2.vcl");
    let orig = fs::read_to_string(&seed_path).unwrap();
    let needle = "set req.http.X-Internal = \"true\";";
    assert!(orig.contains(needle), "fixture assumption changed");
    let changed = orig.replacen(
        needle,
        "set req.http.X-Internal = \"totally-different\";",
        1,
    );

    let tmp = unique_tmp_dir("i4");
    let b_path = tmp.join("seed2_b.vcl");
    fs::write(&b_path, &changed).unwrap();

    let out = run_bin(&[
        "compare",
        "--diff",
        seed_path.to_str().unwrap(),
        b_path.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(1));
    let stdout = stdout_of(&out);
    assert!(
        stdout.contains("totally-different"),
        "diff should contain the changed line:\n{stdout}"
    );
    assert!(
        stdout.contains("@@"),
        "expected a unified-diff hunk header:\n{stdout}"
    );
    // Context radius 3: declarations far from the one-line change (the
    // imports/backends near the top of the file) must not appear.
    assert!(
        !stdout.contains("import directors"),
        "diff should not include unrelated far-away lines:\n{stdout}"
    );
    assert!(
        !stdout.contains(".host ="),
        "diff should not include unrelated far-away lines:\n{stdout}"
    );

    let out = run_bin(&[
        "compare",
        "--names",
        seed_path.to_str().unwrap(),
        b_path.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(1));
    let stdout = stdout_of(&out);
    assert!(
        stdout.contains("names (A"),
        "missing names header for A:\n{stdout}"
    );
    assert!(
        stdout.contains("names (B"),
        "missing names header for B:\n{stdout}"
    );
    // seed2.vcl has a custom probe, a custom acl and a custom sub, all of
    // which get renamed -- the bijection table should mention them.
    assert!(
        stdout.contains("probe"),
        "expected a probe entry:\n{stdout}"
    );
    assert!(stdout.contains("acl"), "expected an acl entry:\n{stdout}");
}

// ─────────────────────────── I5 ───────────────────────────

/// Fixpoint property (C4), driven end-to-end through the CLI: for every
/// seed, `dump` then `print` then re-`dump` the printed output must
/// produce byte-identical canonical JSON.
#[test]
fn i5_fixpoint_over_all_seeds() {
    for &seed in SEEDS {
        let seed_path = corpus(seed);
        let inc_args = include_args_for(seed);

        let mut dump_args: Vec<&str> = vec!["dump"];
        dump_args.extend(inc_args.iter().copied());
        let seed_str = seed_path.to_str().unwrap().to_string();
        dump_args.push(&seed_str);
        let dump1 = run_bin(&dump_args);
        assert_eq!(
            dump1.status.code(),
            Some(0),
            "seed {seed}: initial dump failed\nstderr:{}",
            stderr_of(&dump1)
        );
        let json1 = stdout_of(&dump1);

        let mut print_args: Vec<&str> = vec!["print"];
        print_args.extend(inc_args.iter().copied());
        print_args.push(&seed_str);
        let printed = run_bin(&print_args);
        assert_eq!(
            printed.status.code(),
            Some(0),
            "seed {seed}: print failed\nstderr:{}",
            stderr_of(&printed)
        );

        let tmp = unique_tmp_dir(&format!("i5-{seed}"));
        let reprinted_path = tmp.join(seed);
        fs::write(&reprinted_path, stdout_of(&printed)).unwrap();

        // The printer inlines all `include`s (there's no `Decl::Include` --
        // splicing happens at lex time), so the re-dump never needs -I.
        let dump2 = run_bin(&["dump", reprinted_path.to_str().unwrap()]);
        assert_eq!(
            dump2.status.code(),
            Some(0),
            "seed {seed}: re-dump of printed output failed\nstderr:{}",
            stderr_of(&dump2)
        );
        let json2 = stdout_of(&dump2);

        assert_eq!(
            json1, json2,
            "seed {seed}: fixpoint violated -- normalize(parse(print(normalize(parse(seed))))) != normalize(parse(seed))"
        );
    }
}

// ─────────────────────────── I6 (seed7: split subs) ───────────────────────────

/// seed7.vcl's split-sub fragments composed with `--split-includes`: moving
/// one of its top-level chunks (which, depending on the RNG draw, may well
/// be one of the `sub vcl_recv` fragments themselves) into an included file
/// must not change the comparison result -- split-sub merging and
/// include-splicing are independent normalization concerns.
#[test]
fn i6_seed7_split_sub_composes_with_split_includes() {
    if !mutate_available() {
        eprintln!("skipping i6_seed7_split_sub_composes_with_split_includes: `python3 tools/mutate.py --help` failed");
        return;
    }

    let seed_path = corpus("seed7.vcl");
    let outdir = unique_tmp_dir("i6-seed7-split-includes");

    let status = Command::new("python3")
        .arg("tools/mutate.py")
        .arg(&seed_path)
        .args(["--split-includes", "1", "--seed", "42", "-o"])
        .arg(&outdir)
        .current_dir(manifest_dir())
        .status()
        .expect("failed to run tools/mutate.py");
    assert!(status.success(), "mutate.py --split-includes 1 failed");

    let main_path = outdir.join("seed7.vcl");
    assert!(
        main_path.exists(),
        "expected split main file at {}",
        main_path.display()
    );

    let out = run_bin(&[
        "compare",
        "-I",
        outdir.to_str().unwrap(),
        seed_path.to_str().unwrap(),
        main_path.to_str().unwrap(),
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "seed7 split into an include should still compare equivalent\nstdout:{}\nstderr:{}",
        stdout_of(&out),
        stderr_of(&out)
    );
}

/// `--diff`/`--names` end-to-end on a file with a multi-fragment builtin
/// sub: the divergence output must stay comprehensible (correct changed
/// line, non-empty bijection) when the differing sub is actually merged
/// from several source fragments.
#[test]
fn i7_seed7_diff_and_names_with_multi_fragment_sub() {
    let seed_path = corpus("seed7.vcl");
    let orig = fs::read_to_string(&seed_path).unwrap();
    let needle = "set req.http.X-Trusted = \"true\";";
    assert!(orig.contains(needle), "fixture assumption changed");
    let changed = orig.replacen(needle, "set req.http.X-Trusted = \"yes\";", 1);

    let tmp = unique_tmp_dir("i7-seed7");
    let b_path = tmp.join("seed7_b.vcl");
    fs::write(&b_path, &changed).unwrap();

    let out = run_bin(&[
        "compare",
        "--diff",
        seed_path.to_str().unwrap(),
        b_path.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(1));
    let stdout = stdout_of(&out);
    assert!(
        stdout.contains("yes"),
        "diff should contain the changed line:\n{stdout}"
    );
    assert!(
        stdout.contains("@@"),
        "expected a unified-diff hunk header:\n{stdout}"
    );

    let out = run_bin(&[
        "compare",
        "--names",
        seed_path.to_str().unwrap(),
        b_path.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(1));
    let stdout = stdout_of(&out);
    assert!(
        stdout.contains("names (A"),
        "missing names header for A:\n{stdout}"
    );
    // seed7's custom sub (log_request) and its acl/probe get renamed.
    assert!(stdout.contains("sub"), "expected a sub entry:\n{stdout}");
    assert!(stdout.contains("acl"), "expected an acl entry:\n{stdout}");
}

// ─────────────────────────── I8 ───────────────────────────

/// `--vcl-path` is an equivalent spelling of `-I` (matches varnishd's
/// `-p vcl_path=` parameter name): it must resolve seed4.vcl's includes
/// just as well, standalone or mixed with `-I`.
#[test]
fn i8_vcl_path_is_equivalent_to_dash_i() {
    let seed4 = corpus("seed4.vcl");

    let out = run_bin(&[
        "compare",
        "--vcl-path",
        "tests/corpus",
        seed4.to_str().unwrap(),
        seed4.to_str().unwrap(),
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "--vcl-path alone should resolve seed4's includes\nstdout:{}\nstderr:{}",
        stdout_of(&out),
        stderr_of(&out)
    );

    let out = run_bin(&[
        "compare",
        "-I",
        "tests/corpus",
        "--vcl-path",
        "tests/corpus",
        seed4.to_str().unwrap(),
        seed4.to_str().unwrap(),
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "-I and --vcl-path should be mixable\nstdout:{}\nstderr:{}",
        stdout_of(&out),
        stderr_of(&out)
    );
}

// ─────────────────────────── I9 ───────────────────────────

/// End-to-end (CLI) regression for the header-case bug: writing a header is
/// case-sensitive (different case -> a different header on the wire, NOT
/// equivalent), but reading one is a case-insensitive lookup (different
/// case -> still equivalent).
#[test]
fn i9_header_write_case_sensitive_read_case_insensitive() {
    let tmp = unique_tmp_dir("i9-header-case");

    // Differ only in the case used to WRITE the header -> not equivalent.
    let write_a = tmp.join("write_a.vcl");
    let write_b = tmp.join("write_b.vcl");
    fs::write(
        &write_a,
        "vcl 4.1;\nbackend default { .host = \"127.0.0.1\"; .port = \"80\"; }\nsub vcl_recv {\n    set req.http.foo = \"1\";\n}\n",
    )
    .unwrap();
    fs::write(
        &write_b,
        "vcl 4.1;\nbackend default { .host = \"127.0.0.1\"; .port = \"80\"; }\nsub vcl_recv {\n    set req.http.FOO = \"1\";\n}\n",
    )
    .unwrap();
    let out = run_bin(&[
        "compare",
        write_a.to_str().unwrap(),
        write_b.to_str().unwrap(),
    ]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "different write case should NOT be equivalent\nstdout:{}\nstderr:{}",
        stdout_of(&out),
        stderr_of(&out)
    );

    // Differ only in the case used to READ the header -> still equivalent.
    let read_a = tmp.join("read_a.vcl");
    let read_b = tmp.join("read_b.vcl");
    fs::write(
        &read_a,
        "vcl 4.1;\nbackend default { .host = \"127.0.0.1\"; .port = \"80\"; }\nsub vcl_recv {\n    if (req.http.bar == \"x\") {\n        set req.http.hit = \"1\";\n    }\n}\n",
    )
    .unwrap();
    fs::write(
        &read_b,
        "vcl 4.1;\nbackend default { .host = \"127.0.0.1\"; .port = \"80\"; }\nsub vcl_recv {\n    if (req.http.BAR == \"x\") {\n        set req.http.hit = \"1\";\n    }\n}\n",
    )
    .unwrap();
    let out = run_bin(&[
        "compare",
        read_a.to_str().unwrap(),
        read_b.to_str().unwrap(),
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "different read case should still be equivalent\nstdout:{}\nstderr:{}",
        stdout_of(&out),
        stderr_of(&out)
    );
}

// ─────────────────────────── I10 ───────────────────────────

/// End-to-end (CLI) regression: an unrecognized `vcl_*`-prefixed sub name
/// (`vcl_devliver`, a typo of `vcl_deliver`) is NOT rejected -- only a
/// non-fatal warning on stderr, exit 0. We can't reliably distinguish a
/// real Enterprise/vmod-specific hook name from a typo (see
/// `ast::is_builtin_sub`'s doc comment), so this deliberately favors never
/// rejecting legitimate VCL over catching every typo.
#[test]
fn i10_unrecognized_vcl_prefixed_sub_name_warns_but_succeeds() {
    let tmp = unique_tmp_dir("i10-vcl-reserved");
    let bad = tmp.join("typo_builtin.vcl");
    fs::write(
        &bad,
        "vcl 4.1;\nbackend default none;\nsub vcl_recv {\n    set req.http.x = \"1\";\n}\nsub vcl_devliver {\n    set resp.http.y = \"1\";\n}\n",
    )
    .unwrap();

    let out = run_bin(&["dump", bad.to_str().unwrap()]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "unrecognized vcl_*-prefixed name should warn, not fail\nstdout:{}\nstderr:{}",
        stdout_of(&out),
        stderr_of(&out)
    );
    let stderr = stderr_of(&out);
    assert!(
        stderr.contains("vcl_devliver"),
        "expected a warning naming the unrecognized sub:\n{stderr}"
    );
}

// ─────────────────────────── I11 ───────────────────────────

/// `print` keeps original declared names by default (clearer to read);
/// `--rename` opts into the same canonical `backend_N`/`probe_N`/... names
/// `dump`/`compare` always use.
#[test]
fn i11_print_default_keeps_original_names_rename_flag_opts_in() {
    let seed_path = corpus("seed2.vcl");

    let out = run_bin(&["print", seed_path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "stderr:{}", stderr_of(&out));
    let default_out = stdout_of(&out);
    assert!(
        default_out.contains("backend web01"),
        "default print should keep original backend names:\n{default_out}"
    );
    assert!(
        default_out.contains("acl office_net"),
        "default print should keep original acl names:\n{default_out}"
    );
    assert!(
        !default_out.contains('$'),
        "no internal placeholder name should leak into print output:\n{default_out}"
    );

    let out = run_bin(&["print", "--rename", seed_path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "stderr:{}", stderr_of(&out));
    let renamed_out = stdout_of(&out);
    assert!(
        renamed_out.contains("backend backend_1"),
        "--rename should use canonical backend names:\n{renamed_out}"
    );
    assert!(
        !renamed_out.contains("web01"),
        "--rename should not keep the original backend name:\n{renamed_out}"
    );

    // The default (unrenamed) print output must still be real, re-parseable
    // VCL: `dump` it and confirm it succeeds.
    let tmp = unique_tmp_dir("i11-reparse");
    let printed_path = tmp.join("seed2_printed.vcl");
    fs::write(&printed_path, &default_out).unwrap();
    let out = run_bin(&["dump", printed_path.to_str().unwrap()]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "default print output should still re-parse cleanly\nstderr:{}",
        stderr_of(&out)
    );
}

// ─────────────────────────── I12 ───────────────────────────

/// Builds a `vcl.show`-dump string from `(filename, content)` pairs, with
/// correct byte-length markers computed from `content.len()` -- never
/// hand-counted, so the test itself can't reintroduce the exact class of
/// off-by-one mistake this feature is designed to catch.
fn build_vcl_show(chunks: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (i, (filename, content)) in chunks.iter().enumerate() {
        out.push_str(&format!("// VCL.SHOW {i} {} {filename}\n", content.len()));
        out.push_str(content);
    }
    out
}

/// Root includes child_a and child_b; child_b has its own secondary
/// `vcl 4.1;` (tolerated no-op, see i10-adjacent parser fix); a trailing
/// `Builtin` chunk is never `include`d and must be silently dropped.
fn vcl_show_fixture() -> String {
    let root = "vcl 4.1;\nbackend default none;\ninclude \"child_a.vcl\";\ninclude \"child_b.vcl\";\nsub vcl_recv { call helper_a; call helper_b; return (hash); }\n";
    let child_a = "sub helper_a { set req.http.a = \"1\"; }\n";
    let child_b = "vcl 4.1;\nsub helper_b { set req.http.b = \"2\"; }\n";
    let builtin = "sub vcl_init { return (ok); }\n"; // never reached, must be dropped
    build_vcl_show(&[
        ("/etc/varnish/root.vcl", root),
        ("/etc/varnish/child_a.vcl", child_a),
        ("/etc/varnish/child_b.vcl", child_b),
        ("Builtin", builtin),
    ])
}

#[test]
fn i12a_from_vcl_show_dump_succeeds() {
    let tmp = unique_tmp_dir("i12a");
    let path = tmp.join("dump.txt");
    fs::write(&path, vcl_show_fixture()).unwrap();

    let out = run_bin(&["dump", "--from-vcl-show", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "stderr:{}", stderr_of(&out));
    let stdout = stdout_of(&out);
    assert!(
        stdout.contains("helper_a") || stdout.contains("sub_"),
        "expected helper_a (or its canonical rename) in output:\n{stdout}"
    );
}

#[test]
fn i12b_from_vcl_show_ignores_include_paths() {
    let tmp = unique_tmp_dir("i12b");
    let path = tmp.join("dump.txt");
    fs::write(&path, vcl_show_fixture()).unwrap();

    let out = run_bin(&[
        "dump",
        "--from-vcl-show",
        "-I",
        "/does/not/exist",
        path.to_str().unwrap(),
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "disk lookup must never be attempted with --from-vcl-show\nstderr:{}",
        stderr_of(&out)
    );
    assert!(
        stderr_of(&out).contains("--from-vcl-show"),
        "expected a warning that -I is ignored:\n{}",
        stderr_of(&out)
    );
}

#[test]
fn i12c_from_vcl_show_trivially_equivalent_to_self() {
    let tmp = unique_tmp_dir("i12c");
    let path = tmp.join("dump.txt");
    fs::write(&path, vcl_show_fixture()).unwrap();

    let out = run_bin(&[
        "compare",
        "--from-vcl-show",
        path.to_str().unwrap(),
        path.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(0), "stderr:{}", stderr_of(&out));
    assert!(stdout_of(&out).contains("equivalent"));
}

#[test]
fn i12d_from_vcl_show_with_cartouche() {
    let tmp = unique_tmp_dir("i12d");
    let dashes = "-".repeat(32);
    let dumped = format!("{dashes}\nItem 1: test\n{dashes}\n{}", vcl_show_fixture());
    let path = tmp.join("dump.txt");
    fs::write(&path, &dumped).unwrap();

    let out = run_bin(&["dump", "--from-vcl-show", path.to_str().unwrap()]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "cartouche-prefixed dump should parse identically\nstderr:{}",
        stderr_of(&out)
    );
}

#[test]
fn i12e_from_vcl_show_bad_chunk_length_errors() {
    let tmp = unique_tmp_dir("i12e");
    let dump = vcl_show_fixture();

    // Corrupt chunk 0's declared length: the first marker line is
    // "// VCL.SHOW 0 <len> <filename>" -- split_whitespace gives
    // ["//", "VCL.SHOW", "0", "<len>", "<filename>"]; rewrite just the
    // length field (index 3) to something wrong.
    let real_marker = dump.lines().next().unwrap().to_string();
    let parts: Vec<&str> = real_marker.split_whitespace().collect();
    let wrong_marker = format!("{} {} {} 999999 {}", parts[0], parts[1], parts[2], parts[4]);
    let corrupted = dump.replacen(&real_marker, &wrong_marker, 1);

    let path = tmp.join("dump.txt");
    fs::write(&path, &corrupted).unwrap();

    let out = run_bin(&["dump", "--from-vcl-show", path.to_str().unwrap()]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "corrupted chunk length should error\nstdout:{}\nstderr:{}",
        stdout_of(&out),
        stderr_of(&out)
    );
}

#[test]
fn i12f_from_vcl_show_non_builtin_leftover_errors() {
    let tmp = unique_tmp_dir("i12f");
    // A trailing chunk that is never `include`d and isn't named "Builtin".
    let root = "vcl 4.1;\nbackend default none;\nsub vcl_recv { return (hash); }\n";
    let orphan = "sub never_called { set req.http.x = \"1\"; }\n";
    let dump = build_vcl_show(&[
        ("/etc/varnish/root.vcl", root),
        ("/etc/varnish/orphan.vcl", orphan),
    ]);
    let path = tmp.join("dump.txt");
    fs::write(&path, &dump).unwrap();

    let out = run_bin(&["dump", "--from-vcl-show", path.to_str().unwrap()]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "an unreferenced non-Builtin trailing chunk should error\nstdout:{}\nstderr:{}",
        stdout_of(&out),
        stderr_of(&out)
    );
    assert!(
        stderr_of(&out).contains("unused"),
        "expected an 'unused chunk' error:\n{}",
        stderr_of(&out)
    );
}

// ─────────────────────────── I13 ───────────────────────────

/// Finds a real vmod directory to load `libvmod_blob.so` from, for I13.
/// Tries `pkg-config` first, then a couple of common install locations;
/// `None` if the vmod genuinely isn't available (test skips gracefully).
fn find_blob_vmod_dir() -> Option<PathBuf> {
    if let Ok(out) = Command::new("pkg-config")
        .arg("--variable=vmoddir")
        .arg("varnishapi")
        .output()
    {
        if out.status.success() {
            let dir = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
            if dir.join("libvmod_blob.so").exists() {
                return Some(dir);
            }
        }
    }
    for candidate in ["/usr/lib/varnish/vmods", "/usr/lib64/varnish/vmods"] {
        let dir = PathBuf::from(candidate);
        if dir.join("libvmod_blob.so").exists() {
            return Some(dir);
        }
    }
    None
}

/// End-to-end (CLI) regression for ENUM-typed vmod argument validation
/// against the *real* `libvmod_blob.so`'s JSON spec (not a hand-built test
/// fixture): a legal encoding literal (`HEX`) is silently accepted; an
/// illegal one (`BOGUS`) is a hard error naming the exact legal value list.
#[test]
fn i13_enum_argument_validated_against_real_vmod_spec() {
    let Some(vmod_dir) = find_blob_vmod_dir() else {
        eprintln!("skipping i13: libvmod_blob.so not found on this system");
        return;
    };

    let tmp = unique_tmp_dir("i13-enum");
    let good = tmp.join("good.vcl");
    fs::write(
        &good,
        "vcl 4.1;\nimport blob;\nbackend default none;\nsub vcl_recv {\n    set req.http.x = blob.encode(HEX, blob = req.hash);\n    return (hash);\n}\n",
    )
    .unwrap();
    let out = run_bin(&[
        "dump",
        "--vmod-path",
        vmod_dir.to_str().unwrap(),
        good.to_str().unwrap(),
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "HEX is a legal blob.encode encoding\nstderr:{}",
        stderr_of(&out)
    );
    assert!(
        !stderr_of(&out).contains("HEX"),
        "a valid enum literal should not warn:\n{}",
        stderr_of(&out)
    );

    let bad = tmp.join("bad.vcl");
    fs::write(
        &bad,
        "vcl 4.1;\nimport blob;\nbackend default none;\nsub vcl_recv {\n    set req.http.x = blob.encode(BOGUS, blob = req.hash);\n    return (hash);\n}\n",
    )
    .unwrap();
    let out = run_bin(&[
        "dump",
        "--vmod-path",
        vmod_dir.to_str().unwrap(),
        bad.to_str().unwrap(),
    ]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "BOGUS is not a legal blob.encode encoding\nstdout:{}",
        stdout_of(&out)
    );
    let stderr = stderr_of(&out);
    assert!(stderr.contains("BOGUS"), "{stderr}");
    assert!(
        stderr.contains("HEX"),
        "expected the legal value list in the error:\n{stderr}"
    );
}
