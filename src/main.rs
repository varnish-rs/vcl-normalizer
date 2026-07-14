mod ast;
mod canon;
mod compare;
mod lexer;
mod normalize;
mod parser;
mod printer;
mod symbols;
mod vclshow;
mod vmod;

use clap::{Args, Parser, Subcommand};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use vmod::VmodSpec;

/// VCL 4.1 functional-equivalence comparator.
///
/// Note: `varnishd -C` remains the authoritative VCL compiler; this tool
/// answers *equivalence*, not full *validity*.
#[derive(Parser)]
#[command(name = "vcl-normalizer", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Args)]
struct Common {
    /// VCL include search path, repeatable
    #[arg(short = 'I', long = "vcl-path", value_name = "DIR")]
    include: Vec<PathBuf>,

    /// Vmod .so search path, repeatable; default from `pkg-config --variable=vmoddir varnishapi`.
    #[arg(long = "vmod-path", value_name = "DIR")]
    vmod_path: Vec<PathBuf>,

    /// Skip vmod spec loading entirely (structural vmod handling).
    #[arg(long)]
    no_vmod: bool,

    /// Parse `<FILE>` as a `varnishadm vcl.show -v` dump (captured e.g. by
    /// varnishgather) instead of a plain VCL file. Every `include` is
    /// resolved against the other files already present in the dump, in
    /// document order -- the real filesystem and `-I`/`--vcl-path` are
    /// never consulted.
    #[arg(long = "from-vcl-show")]
    from_vcl_show: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print the canonical (normalized) AST as JSON.
    Dump {
        #[command(flatten)]
        common: Common,
        file: PathBuf,
    },
    /// Pretty-print the normalized VCL. By default, original backend/probe/acl/sub names are kept (clearer to read); pass `--rename` to see the
    /// canonical `backend_1`/`sub_2`/... names `compare` uses internally.
    Print {
        #[command(flatten)]
        common: Common,

        /// Apply canonical renaming (as `dump`/`compare` always do) instead
        /// of keeping the original declared names.
        #[arg(long)]
        rename: bool,

        /// Omit source comments from the output (by default, `print`
        /// re-emits them next to the code they were attached to).
        #[arg(long)]
        no_comments: bool,

        file: PathBuf,
    },
    /// Compare two VCL files for functional equivalence.
    Compare {
        #[command(flatten)]
        common: Common,

        /// Also print a unified diff of the two canonical pretty-prints.
        #[arg(long)]
        diff: bool,

        /// Cap on reported divergences.
        #[arg(long, default_value_t = 20)]
        max_reports: usize,

        /// Print the discovered name bijection.
        #[arg(long)]
        names: bool,

        file_a: PathBuf,
        file_b: PathBuf,
    },
}

/// Error from the shared parse/validate/normalize pipeline, tagged with
/// enough context to render diagnostics and pick the right exit code.
enum PipelineError {
    /// File-system / usage problem (missing file, unreadable, etc). Exit 3.
    Io(String),
    /// Failure inside `parser::parse_file` itself. Note: `parser::parse_file`
    /// (and the `lexer::lex` it wraps) only returns a `SourceMap` on the
    /// `Ok` path -- on `Err` the partially-built map is dropped inside the
    /// function, so we cannot fully resolve spans that point into included
    /// files for these errors. We fall back to re-reading the root file
    /// ourselves for the common case (error span in the root file); see
    /// `render_parse_error`. Not attempted for `--from-vcl-show` (see
    /// `from_vcl_show` field below): a span's file-0 offsets there are
    /// relative to a *chunk's* content, not the raw dump file's bytes, so
    /// re-reading the dump directly would render a misleading caret rather
    /// than a merely-missing one. Exit 2.
    Parse {
        path: PathBuf,
        err: ast::VclError,
        from_vcl_show: bool,
    },
    /// Failure in `symbols::validate` or `normalize::normalize`, which run
    /// after a successful parse, so the real `SourceMap` is available. Exit 2.
    Semantic {
        sm: ast::SourceMap,
        err: ast::VclError,
    },
}

/// Runs the shared pipeline (parse -> load vmods -> validate -> normalize)
/// for a single file, per Common's flags. `rename` controls whether
/// normalize pass 4 actually renames declarations (`dump`/`compare` always
/// pass `true`; `print` passes its `--rename` flag, default `false`).
fn build(
    path: &Path,
    common: &Common,
    rename: bool,
) -> Result<(ast::Program, ast::SourceMap, ast::NameMap), PipelineError> {
    if !path.exists() {
        return Err(PipelineError::Io(format!(
            "error: no such file: {}",
            path.display()
        )));
    }

    if common.from_vcl_show && !common.include.is_empty() {
        eprintln!("warning: -I/--vcl-path is ignored with --from-vcl-show");
    }

    let (mut program, sm) = if common.from_vcl_show {
        parser::parse_vcl_show_file(path).map_err(|err| PipelineError::Parse {
            path: path.to_path_buf(),
            err,
            from_vcl_show: true,
        })?
    } else {
        parser::parse_file(path, &common.include).map_err(|err| PipelineError::Parse {
            path: path.to_path_buf(),
            err,
            from_vcl_show: false,
        })?
    };

    let vmod_paths = if !common.vmod_path.is_empty() {
        common.vmod_path.clone()
    } else {
        vmod::default_vmod_paths()
    };

    let mut specs: BTreeMap<String, VmodSpec> = BTreeMap::new();
    if !common.no_vmod {
        for decl in &program.decls {
            if let ast::Decl::Import { name, from, .. } = decl {
                if let Some(spec) = vmod::load_vmod(name, from.as_deref(), &vmod_paths) {
                    specs.insert(name.clone(), spec);
                }
            }
        }
    }

    if let Err(err) = symbols::validate(&program, &specs) {
        return Err(PipelineError::Semantic { sm, err });
    }

    let names = match normalize::normalize(&mut program, &specs, rename) {
        Ok(names) => names,
        Err(err) => return Err(PipelineError::Semantic { sm, err }),
    };

    Ok((program, sm, names))
}

/// Best-effort rendering for a parse error, which arrives without its
/// `SourceMap` (see `PipelineError::Parse`). Re-reads the root file so
/// errors whose span lies in it (the overwhelming common case) still get
/// full `file:line:col` + caret rendering; anything else falls back to
/// the bare message rather than risk an out-of-bounds panic. Skipped
/// entirely for `--from-vcl-show`: `path` there is the raw dump file, whose
/// bytes don't line up with a chunk-relative span (see `PipelineError::Parse`
/// doc comment) -- re-reading it would render a misleading, not just
/// missing, caret.
fn render_parse_error(path: &Path, err: &ast::VclError, from_vcl_show: bool) -> String {
    if !from_vcl_show {
        if let Some(span) = err.span {
            if span.file == 0 {
                if let Ok(content) = std::fs::read_to_string(path) {
                    if span.lo as usize <= content.len() && span.hi as usize <= content.len() {
                        let mut sm = ast::SourceMap::default();
                        sm.add(path.to_path_buf(), content);
                        return err.render(&sm);
                    }
                }
            }
        }
    }
    err.msg.clone()
}

fn report_pipeline_error(e: PipelineError) -> ExitCode {
    match e {
        PipelineError::Io(msg) => {
            eprintln!("{msg}");
            ExitCode::from(3)
        }
        PipelineError::Parse {
            path,
            err,
            from_vcl_show,
        } => {
            eprintln!("{}", render_parse_error(&path, &err, from_vcl_show));
            ExitCode::from(2)
        }
        PipelineError::Semantic { sm, err } => {
            eprintln!("{}", err.render(&sm));
            ExitCode::from(2)
        }
    }
}

fn print_names(a: &ast::NameMap, b: &ast::NameMap) {
    println!("names (A: canonical <- original):");
    for (kind, canonical, original) in &a.entries {
        println!("  {kind:<10} {canonical:<24} <- {original}");
    }
    println!("names (B: canonical <- original):");
    for (kind, canonical, original) in &b.entries {
        println!("  {kind:<10} {canonical:<24} <- {original}");
    }
}

// Exit codes: 0 equivalent · 1 not equivalent · 2 parse/validation error · 3 usage/IO error
fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            // clap "errors" include --help/--version, which must exit 0.
            if e.use_stderr() {
                eprint!("{e}");
                return ExitCode::from(3);
            }
            print!("{e}");
            return ExitCode::SUCCESS;
        }
    };

    match cli.cmd {
        Cmd::Dump { common, file } => match build(&file, &common, true) {
            Ok((program, _sm, _names)) => {
                println!("{}", canon::to_string(&program));
                ExitCode::SUCCESS
            }
            Err(e) => report_pipeline_error(e),
        },
        Cmd::Print {
            common,
            rename,
            no_comments,
            file,
        } => match build(&file, &common, rename) {
            Ok((mut program, _sm, _names)) => {
                if no_comments {
                    program.comments = ast::CommentMap::default();
                    program.trailing_comments = Vec::new();
                }
                println!("{}", printer::print(&program));
                ExitCode::SUCCESS
            }
            Err(e) => report_pipeline_error(e),
        },
        Cmd::Compare {
            common,
            diff,
            max_reports,
            names,
            file_a,
            file_b,
        } => {
            let (a, sm_a, names_a) = match build(&file_a, &common, true) {
                Ok(v) => v,
                Err(e) => return report_pipeline_error(e),
            };
            let (b, sm_b, names_b) = match build(&file_b, &common, true) {
                Ok(v) => v,
                Err(e) => return report_pipeline_error(e),
            };

            if canon::canon_eq(&a, &b) {
                println!("equivalent");
                return ExitCode::SUCCESS;
            }

            let divs = compare::compare(&a, &b, max_reports);
            let capped = divs.len() >= max_reports;
            print!("{}", compare::render_report(&divs, &sm_a, &sm_b, capped));

            if diff {
                let print_a = printer::print(&a);
                let print_b = printer::print(&b);
                let text_diff = similar::TextDiff::from_lines(print_a.as_str(), print_b.as_str());
                println!("{}", text_diff.unified_diff().context_radius(3));
            }

            if names {
                print_names(&names_a, &names_b);
            }

            ExitCode::from(1)
        }
    }
}
