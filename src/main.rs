use ariadne::{Label, Report, ReportKind, Source};
use trace::pipeline::{self, StageError};
use trace::{fmt, lexer, parser, resolve};

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--lsp") {
        return trace::lsp::run();
    }
    let explain = args.iter().any(|a| a == "--explain-schedule");
    let show_closures = args.iter().any(|a| a == "--closures");
    let show_elaborate = args.iter().any(|a| a == "--elaborate");
    let show_lower = args.iter().any(|a| a == "--lower");
    let show_firrtl = args.iter().any(|a| a == "--firrtl");
    let show_fmt = args.iter().any(|a| a == "--fmt");
    let write_fmt = args.iter().any(|a| a == "--write" || a == "-w");
    let Some(path) = args
        .iter()
        .find(|a| !a.starts_with('-') || a.as_str() == "-")
    else {
        eprintln!(
            "usage: trace <file.tr | -> [--explain-schedule] [--closures] [--elaborate] \
             [--lower] [--firrtl] [--fmt [--write]]\n       trace --lsp"
        );
        return std::process::ExitCode::FAILURE;
    };
    let path = path.clone();
    let stdin = path == "-";
    let src = if stdin {
        let mut buf = String::new();
        match std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf) {
            Ok(_) => buf,
            Err(e) => {
                eprintln!("trace: cannot read stdin: {e}");
                return std::process::ExitCode::FAILURE;
            }
        }
    } else {
        match std::fs::read_to_string(&path) {
            Ok(src) => src,
            Err(e) => {
                eprintln!("trace: cannot read {path}: {e}");
                return std::process::ExitCode::FAILURE;
            }
        }
    };

    let (tokens, lex_errors) = lexer::lex(&src);
    for err in &lex_errors {
        report(&path, &src, err.span.clone(), "unrecognized character(s)");
    }
    if !lex_errors.is_empty() {
        return std::process::ExitCode::FAILURE;
    }

    let (ast, parse_errors) = parser::parse(&src, &tokens);
    for err in &parse_errors {
        report(&path, &src, err.span.clone(), &err.message);
    }
    if !parse_errors.is_empty() {
        return std::process::ExitCode::FAILURE;
    }

    if show_fmt {
        let formatted = fmt::format(&src, &tokens);
        if write_fmt && !stdin {
            if let Err(e) = std::fs::write(&path, &formatted) {
                eprintln!("trace: cannot write {path}: {e}");
                return std::process::ExitCode::FAILURE;
            }
        } else {
            print!("{formatted}");
        }
        return std::process::ExitCode::SUCCESS;
    }

    if !explain && !show_closures && !show_elaborate && !show_lower && !show_firrtl {
        print!("{}", ast.dump());
    }

    let (res, resolve_errors) = resolve::resolve(&ast);
    for err in &resolve_errors {
        report(&path, &src, err.span.clone(), &err.message);
    }
    if !resolve_errors.is_empty() {
        return std::process::ExitCode::FAILURE;
    }

    // Every stage from here on is a splice-and-reparse round trip
    // (closures -> elaborate -> lower), each producing real trace SOURCE
    // text that the next stage's `check` re-runs the whole frontend
    // over — `pipeline.rs` is the ONE place this chain lives, shared
    // with `tests/sim.rs`'s own use of it, so this CLI path and the
    // tested path can never silently diverge (this is also, as of this
    // change, the first time `--firrtl` has ever actually run past
    // `resolve`/`effects` on the ORIGINAL source — every `<elaborates>`/
    // closure-using example used to hard-error here, a pre-existing gap
    // this closes rather than a regression).
    let resolved = pipeline::Resolved { ast, res };

    let closures_src = match pipeline::splice_closures(&resolved, &src) {
        Ok(s) => s,
        Err(errs) => return fail(&path, &src, &errs),
    };
    if show_closures {
        print!("{closures_src}");
        return std::process::ExitCode::SUCCESS;
    }

    let checked1 = match pipeline::check(&closures_src) {
        Ok(c) => c,
        Err(errs) => return fail(&path, &closures_src, &errs),
    };

    let elaborated_src = match pipeline::splice_elaborate(&checked1, &closures_src) {
        Ok(s) => s,
        Err(errs) => return fail(&path, &closures_src, &errs),
    };
    if show_elaborate {
        print!("{elaborated_src}");
        return std::process::ExitCode::SUCCESS;
    }

    let checked2 = match pipeline::check(&elaborated_src) {
        Ok(c) => c,
        Err(errs) => return fail(&path, &elaborated_src, &errs),
    };

    let lowered_src = match pipeline::splice_lower(&checked2, &elaborated_src) {
        Ok(s) => s,
        Err(errs) => return fail(&path, &elaborated_src, &errs),
    };
    if show_lower {
        print!("{lowered_src}");
        return std::process::ExitCode::SUCCESS;
    }

    let checked3 = match pipeline::check(&lowered_src) {
        Ok(c) => c,
        Err(errs) => return fail(&path, &lowered_src, &errs),
    };

    let scheduled = match pipeline::schedule_checked(&checked3) {
        Ok(s) => s,
        Err(errs) => return fail(&path, &lowered_src, &errs),
    };
    if explain {
        print!("{}", scheduled.sched.explain(&checked3.ast, &checked3.res));
    }
    if show_firrtl {
        match pipeline::emit(&checked3, &scheduled) {
            Ok(text) => print!("{text}"),
            Err(errs) => return fail(&path, &lowered_src, &errs),
        }
    }
    std::process::ExitCode::SUCCESS
}

fn fail(path: &str, src: &str, errors: &[StageError]) -> std::process::ExitCode {
    for e in errors {
        report(path, src, e.span.clone(), &e.message);
    }
    std::process::ExitCode::FAILURE
}

/// Splits a trailing hint clause (`"...; use \`trunc(value, 8)\`"`) off a
/// diagnostic message, so `report` can show the main text above the
/// source snippet and the hint once, near the underlined span, instead
/// of repeating the whole message in both places. Only a `"; use "`
/// clause counts as a hint — most error messages chain several `"; "`
/// clauses of ordinary explanation (v0-restriction detail, etc.), not a
/// "here's what to do" suggestion, so this deliberately doesn't split
/// on every semicolon.
fn split_hint(message: &str) -> (&str, Option<&str>) {
    match message.rfind("; use ") {
        Some(idx) => (&message[..idx], Some(&message[idx + 2..])),
        None => (message, None),
    }
}

fn report(path: &str, src: &str, span: lexer::Span, message: &str) {
    let (main, hint) = split_hint(message);
    // Without an attached message, ariadne draws no underline at all —
    // so the no-hint case still needs a label message to point at the
    // span; only when there's a hint to show instead does the label
    // switch to that (avoiding the "full message twice" duplication),
    // rather than always falling back to repeating `main`.
    let label = Label::new((path, span.clone())).with_message(hint.unwrap_or(main));
    Report::build(ReportKind::Error, (path, span))
        .with_message(main)
        .with_label(label)
        .finish()
        .eprint((path, Source::from(src)))
        .expect("failed to print diagnostic");
}

#[cfg(test)]
mod tests {
    use super::split_hint;

    #[test]
    fn splits_a_trailing_use_hint() {
        let (main, hint) =
            split_hint("state write would silently truncate [16] to [8]; use `trunc(value, 8)`");
        assert_eq!(main, "state write would silently truncate [16] to [8]");
        assert_eq!(hint, Some("use `trunc(value, 8)`"));
    }

    #[test]
    fn splits_the_logical_not_bits_1_hint() {
        // Regression test: this message originally read "...got [8]
        // (use `~` for...)" — a parenthetical, not a `"; use "` clause —
        // so `split_hint` never found it and the full message repeated
        // in both the header and the underline label (Lumi caught this
        // by eye, same double-repeat `split_hint`'s own doc comment
        // exists specifically to avoid).
        let (main, hint) = split_hint(
            "`not` needs a [1] operand, got [8]; use `~` for a bitwise complement \
             of a wider value, or compare explicitly",
        );
        assert_eq!(main, "`not` needs a [1] operand, got [8]");
        assert_eq!(
            hint,
            Some("use `~` for a bitwise complement of a wider value, or compare explicitly")
        );
    }

    #[test]
    fn leaves_a_message_with_no_hint_intact() {
        let (main, hint) = split_hint("this call is not yet supported in FIRRTL emission");
        assert_eq!(main, "this call is not yet supported in FIRRTL emission");
        assert_eq!(hint, None);
    }

    #[test]
    fn does_not_treat_an_ordinary_semicolon_clause_as_a_hint() {
        // "; not supported for a depth-1 fifo ... : split into two rules"
        // is ordinary explanatory detail, not a "use X" suggestion — the
        // whole thing stays the main message.
        let (main, hint) = split_hint(
            "this rule both enqueues and dequeues `f` in the same cycle; not supported \
             for a depth-1 fifo (v0 restriction): split into two rules",
        );
        assert_eq!(
            main,
            "this rule both enqueues and dequeues `f` in the same cycle; not supported \
             for a depth-1 fifo (v0 restriction): split into two rules"
        );
        assert_eq!(hint, None);
    }

    #[test]
    fn splits_the_fails_not_declared_hint() {
        // Regression test: this message originally read "...in its body) \
        // but does not declare `<fails>` — add it to this item's own \
        // effect list" — an em-dash clause, not a `"; use "` one — so it
        // printed twice in full, same class of bug as the `not` hint above.
        let (main, hint) = split_hint(
            "`Chk` can fail (a guard, a fifo operation, or a call to failing code \
             somewhere in its body); use `<fails>` in this item's own effect list \
             to declare it",
        );
        assert_eq!(
            main,
            "`Chk` can fail (a guard, a fifo operation, or a call to failing code \
             somewhere in its body)"
        );
        assert_eq!(
            hint,
            Some("use `<fails>` in this item's own effect list to declare it")
        );
    }
}
