//! pbtrim — the thin command-line shell over the pbpp library.
//!
//! Everything here is I/O and presentation: reading files, walking
//! directories, printing reports. All semantics live in the library.
//!
//! Exit codes: `0` success, `1` only for `fmt --check` drift, `2` any
//! error (usage, parse, rules, I/O). A broken output pipe (e.g. piping
//! into `head`) exits `0` rather than panicking.

use pbpp::sema::SymKind;
use pbpp::{Mark, Pipeline};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "\
pbtrim: proto3 preprocessor

USAGE:
    pbtrim fmt [--check] [--stdout] [--] <files...>
    pbtrim select --rules <file> --root <dir>
    pbtrim prune --rules <file> --root <dir> --out <dir>
    pbtrim --help | --version

SUBCOMMANDS:
    fmt     Reformat proto files in place.
            --check   don't write; exit 1 if any file would change
            --stdout  print the formatted output instead of writing
            --        treat every following argument as a file name

    select  Evaluate selector rules against all .proto files under the root
            and report the computed keep set (nothing is modified).

    prune   Materialize the selection: write trimmed, formatted files to the
            output directory, preserving relative paths. The directory is
            synced under an exclusive lock: files with nothing kept are not
            written, and files a previous run recorded in .pbtrim-manifest
            but this run did not produce are removed (foreign files are
            never touched).
";

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        // A downstream reader closed the pipe: finish quietly, the
        // conventional shell behavior, not a panic/exit-101.
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(io::stderr(), "error: {e}");
            ExitCode::from(2)
        }
    }
}

fn run() -> io::Result<ExitCode> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("fmt") => fmt(&args[1..]),
        Some("select") => select(&args[1..]),
        Some("prune") => prune(&args[1..]),
        Some("--help" | "-h" | "help") => {
            let mut o = io::stdout().lock();
            write!(o, "{USAGE}")?;
            Ok(ExitCode::SUCCESS)
        }
        Some("--version" | "-V") => {
            let mut o = io::stdout().lock();
            writeln!(o, "pbtrim {}", env!("CARGO_PKG_VERSION"))?;
            Ok(ExitCode::SUCCESS)
        }
        Some(other) => {
            eprintln!("error: unknown subcommand `{other}`\n\n{USAGE}");
            Ok(ExitCode::from(2))
        }
        None => {
            eprintln!("{USAGE}");
            Ok(ExitCode::from(2))
        }
    }
}

/// True for the help flags accepted inside every subcommand.
fn is_help(a: &str) -> bool {
    a == "--help" || a == "-h"
}

// ---- fmt ------------------------------------------------------------------

fn fmt(args: &[String]) -> io::Result<ExitCode> {
    let mut check = false;
    let mut stdout = false;
    let mut files: Vec<&str> = Vec::new();
    let mut rest_are_files = false;
    for a in args {
        if rest_are_files {
            files.push(a);
            continue;
        }
        match a.as_str() {
            "--" => rest_are_files = true,
            "--check" => check = true,
            "--stdout" => stdout = true,
            _ if is_help(a) => {
                let mut o = io::stdout().lock();
                write!(o, "{USAGE}")?;
                return Ok(ExitCode::SUCCESS);
            }
            _ if a.starts_with('-') => {
                eprintln!("error: unknown flag `{a}`\n\n{USAGE}");
                return Ok(ExitCode::from(2));
            }
            _ => files.push(a),
        }
    }
    if check && stdout {
        eprintln!("error: --check and --stdout are mutually exclusive\n\n{USAGE}");
        return Ok(ExitCode::from(2));
    }
    if files.is_empty() {
        eprintln!("error: no input files\n\n{USAGE}");
        return Ok(ExitCode::from(2));
    }

    let mut out = io::stdout().lock();
    let mut would_change = false;
    let mut failed = false;
    for path in files {
        let src = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: cannot read {path}: {e}");
                failed = true;
                continue;
            }
        };
        let file = match pbpp::parse(&src) {
            Ok(f) => f,
            Err(e) => {
                eprint!("{}", e.with_file(path));
                failed = true;
                continue;
            }
        };
        let formatted = pbpp::format(&file);
        if stdout {
            out.write_all(formatted.as_bytes())?;
            continue;
        }
        if formatted == src {
            continue;
        }
        would_change = true;
        if check {
            writeln!(out, "would reformat: {path}")?;
        } else if let Err(e) = pbpp::fs::write_atomic(Path::new(path), &formatted) {
            eprintln!("error: cannot write {path}: {e}");
            failed = true;
        }
    }

    Ok(if failed {
        ExitCode::from(2)
    } else if check && would_change {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

// ---- select / prune ----------------------------------------------------------

struct PipelineArgs<'a> {
    rules_path: &'a str,
    root: &'a str,
    out: Option<&'a str>,
}

/// Parses shared `--rules`/`--root` (`--out` for prune) arguments.
///
/// `Ok(Ok(args))` parsed; `Ok(Err(code))` is a handled usage/help exit;
/// the outer `io::Result` only carries help-output pipe errors.
fn parse_pipeline_args<'a>(
    cmd: &str,
    args: &'a [String],
) -> io::Result<Result<PipelineArgs<'a>, ExitCode>> {
    let mut rules_path: Option<&str> = None;
    let mut root: Option<&str> = None;
    let mut out: Option<&str> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--rules" => rules_path = it.next().map(String::as_str),
            "--root" => root = it.next().map(String::as_str),
            "--out" if cmd == "prune" => out = it.next().map(String::as_str),
            _ if is_help(a) => {
                let mut o = io::stdout().lock();
                write!(o, "{USAGE}")?;
                return Ok(Err(ExitCode::SUCCESS));
            }
            other => {
                eprintln!("error: unexpected argument `{other}`\n\n{USAGE}");
                return Ok(Err(ExitCode::from(2)));
            }
        }
    }
    let (Some(rules_path), Some(root)) = (rules_path, root) else {
        eprintln!("error: `{cmd}` requires --rules and --root\n\n{USAGE}");
        return Ok(Err(ExitCode::from(2)));
    };
    if cmd == "prune" && out.is_none() {
        eprintln!("error: `prune` requires --out\n\n{USAGE}");
        return Ok(Err(ExitCode::from(2)));
    }
    Ok(Ok(PipelineArgs {
        rules_path,
        root,
        out,
    }))
}

/// Reads the rules file and every `.proto` under the root (well-known files
/// excluded — they resolve from the builtin table). Discovery goes through
/// `pbpp::fs` (sorted, symlink-refusing, path-validated).
fn load_inputs(a: &PipelineArgs<'_>) -> Result<(String, Vec<(String, String)>), ExitCode> {
    let rules_src = match std::fs::read_to_string(a.rules_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: cannot read {}: {e}", a.rules_path);
            return Err(ExitCode::from(2));
        }
    };

    let mut inputs = match pbpp::fs::discover(Path::new(a.root)) {
        Ok(v) => v,
        Err(e) => {
            eprint!("{e}");
            return Err(ExitCode::from(2));
        }
    };
    inputs.retain(|(rel, _)| {
        let wkt = pbpp::wkt::is_google_protobuf_path(rel);
        if wkt {
            eprintln!("note: skipping {rel} (google/protobuf/* never participates as input)");
        }
        !wkt
    });
    if inputs.is_empty() {
        eprintln!("error: no .proto files found under {}", a.root);
        return Err(ExitCode::from(2));
    }
    Ok((rules_src, inputs))
}

fn build<'a>(
    rules_path: &str,
    rules_src: &'a str,
    inputs: &'a [(String, String)],
) -> Result<(pbpp::rules::RuleSet, Pipeline<'a>), ExitCode> {
    let rules = match pbpp::rules::parse_rules(rules_src) {
        Ok(r) => r,
        Err(e) => {
            eprint!("{}", e.with_file(rules_path));
            return Err(ExitCode::from(2));
        }
    };
    let pipeline = match Pipeline::new(
        inputs
            .iter()
            .map(|(p, s)| (p.clone(), s.as_str()))
            .collect(),
    ) {
        Ok(p) => p,
        Err(e) => {
            eprint!("{e}");
            return Err(ExitCode::from(2));
        }
    };
    Ok((rules, pipeline))
}

fn select(args: &[String]) -> io::Result<ExitCode> {
    let a = match parse_pipeline_args("select", args)? {
        Ok(a) => a,
        Err(c) => return Ok(c),
    };
    let (rules_src, inputs) = match load_inputs(&a) {
        Ok(v) => v,
        Err(c) => return Ok(c),
    };
    let (rules, pipeline) = match build(a.rules_path, &rules_src, &inputs) {
        Ok(v) => v,
        Err(c) => return Ok(c),
    };
    let selected = match pipeline.select(&rules) {
        Ok(s) => s,
        Err(e) => {
            // Rule-located diagnostics carry spans into the rules source
            // but no file name; errors already located in a proto keep
            // their own name.
            eprint!("{}", e.with_file(a.rules_path));
            return Ok(ExitCode::from(2));
        }
    };
    let mut out = io::stdout().lock();
    print_report(&mut out, &pipeline, &selected, &rules)?;
    Ok(ExitCode::SUCCESS)
}

fn prune(args: &[String]) -> io::Result<ExitCode> {
    let a = match parse_pipeline_args("prune", args)? {
        Ok(a) => a,
        Err(c) => return Ok(c),
    };
    let out_dir = PathBuf::from(a.out.unwrap());
    if let Err(c) = check_out_root_disjoint(&out_dir, Path::new(a.root)) {
        return Ok(c);
    }
    let (rules_src, inputs) = match load_inputs(&a) {
        Ok(v) => v,
        Err(c) => return Ok(c),
    };
    let (rules, pipeline) = match build(a.rules_path, &rules_src, &inputs) {
        Ok(v) => v,
        Err(c) => return Ok(c),
    };
    let mut output = match pipeline.prune(&rules) {
        Ok(o) => o,
        Err(e) => {
            eprint!("{}", e.with_file(a.rules_path));
            return Ok(ExitCode::from(2));
        }
    };
    output.format();

    // Manifest-tracked sync under an exclusive lock: writes are atomic,
    // and only files a previous pbtrim run recorded (and this one did not
    // produce) are removed — never foreign files.
    let report = match pbpp::fs::sync(&out_dir, &output.files) {
        Ok(r) => r,
        Err(e) => {
            eprint!("{e}");
            return Ok(ExitCode::from(2));
        }
    };
    let mut out = io::stdout().lock();
    for w in &report.written {
        writeln!(out, "wrote {}", out_dir.join(w).display())?;
    }
    for d in &output.dropped {
        writeln!(out, "dropped {d}")?;
    }
    for r in &report.removed {
        writeln!(out, "removed stale {}", out_dir.join(r).display())?;
    }
    Ok(ExitCode::SUCCESS)
}

/// The output directory must not equal, contain, or live inside the root:
/// pruning would otherwise re-read its own products or delete sources.
fn check_out_root_disjoint(out_dir: &Path, root: &Path) -> Result<(), ExitCode> {
    let canon_root = match root.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: cannot resolve {}: {e}", root.display());
            return Err(ExitCode::from(2));
        }
    };
    if let Err(e) = std::fs::create_dir_all(out_dir) {
        eprintln!("error: cannot create {}: {e}", out_dir.display());
        return Err(ExitCode::from(2));
    }
    let canon_out = match out_dir.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: cannot resolve {}: {e}", out_dir.display());
            return Err(ExitCode::from(2));
        }
    };
    if canon_out.starts_with(&canon_root) || canon_root.starts_with(&canon_out) {
        eprintln!(
            "error: --out ({}) and --root ({}) must be disjoint directories",
            canon_out.display(),
            canon_root.display()
        );
        return Err(ExitCode::from(2));
    }
    Ok(())
}

// ---- report ---------------------------------------------------------------

fn print_report(
    out: &mut impl Write,
    pipeline: &Pipeline<'_>,
    sel: &pbpp::Selected,
    rules: &pbpp::rules::RuleSet,
) -> io::Result<()> {
    let sema = pipeline.sema();
    for (fi, f) in pipeline.file_set().files.iter().enumerate() {
        let tops = &sema.file_top[fi];
        let any_kept = tops.iter().any(|&t| subtree_kept(sema, sel, t));
        if any_kept {
            writeln!(out, "file {}:", f.path)?;
            for &t in tops {
                report_sym(out, sema, sel, rules, t, 1)?;
            }
        } else {
            writeln!(out, "file {}: dropped entirely", f.path)?;
        }
    }
    Ok(())
}

fn subtree_kept(sema: &pbpp::Sema<'_>, sel: &pbpp::Selected, s: pbpp::SymId) -> bool {
    sel.is_kept(s) || sema.children(s).iter().any(|&c| subtree_kept(sema, sel, c))
}

fn report_sym(
    out: &mut impl Write,
    sema: &pbpp::Sema<'_>,
    sel: &pbpp::Selected,
    rules: &pbpp::rules::RuleSet,
    s: pbpp::SymId,
    depth: usize,
) -> io::Result<()> {
    let sym = sema.sym(s);
    let indent = "  ".repeat(depth);

    if sel.is_cascade_dropped(s) {
        let reserved = sym
            .number
            .map(|n| format!("; number {n} reserved"))
            .unwrap_or_default();
        return writeln!(
            out,
            "{indent}drop {} {} (cascade: type excluded{reserved})",
            sym.kind.label(),
            sema.fq(s)
        );
    }

    match sel.mark(s) {
        Mark::None => {
            // Dropped content inside a kept parent is worth reporting; a
            // dropped subtree is reported once at its root.
            let parent_kept = sym.parent.is_some_and(|p| sel.mark(p) >= Mark::Required);
            if parent_kept && !sym.kind.is_def() {
                let reserved = sym
                    .number
                    .map(|n| format!("; number {n} reserved"))
                    .unwrap_or_default();
                writeln!(
                    out,
                    "{indent}drop {} {}{reserved}",
                    sym.kind.label(),
                    sema.fq(s)
                )?;
            } else if sym.kind.is_def() && subtree_kept(sema, sel, s) {
                // Not kept itself but has kept descendants — recurse.
                for &c in sema.children(s) {
                    report_sym(out, sema, sel, rules, c, depth + 1)?;
                }
            }
        }
        mark => {
            let why = match mark {
                Mark::Explicit => sel.deciding_rule(s).map_or_else(
                    || "explicit".to_string(),
                    |ri| format!("explicit: `{}`", rules.rules[ri].raw),
                ),
                Mark::Required => sel.introduced_by(s).map_or_else(
                    || "required".to_string(),
                    |b| format!("required via {}", sema.fq(b)),
                ),
                Mark::Container => "container (holds kept descendants)".to_string(),
                // `Mark::None` is handled by the outer match; the wildcard
                // also satisfies `Mark`'s #[non_exhaustive] contract.
                _ => unreachable!("outer match already handled Mark::None"),
            };
            // Fields/values/methods inside a fully kept parent are implied;
            // report them only when they are the interesting ones.
            let parent_full = sym.parent.is_some_and(|p| sel.mark(p) >= Mark::Required);
            let interesting = sym.kind.is_def() || !parent_full || mark == Mark::Explicit;
            if interesting
                && !(matches!(
                    sym.kind,
                    SymKind::Field | SymKind::EnumValue | SymKind::Method
                ) && parent_full
                    && mark == Mark::Required)
            {
                writeln!(
                    out,
                    "{indent}keep {} {} ({why})",
                    sym.kind.label(),
                    sema.fq(s)
                )?;
            }
            for &c in sema.children(s) {
                report_sym(out, sema, sel, rules, c, depth + 1)?;
            }
        }
    }
    Ok(())
}
