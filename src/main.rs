use ariadne::{Label, Report, ReportKind, Source};
use trace::{effects, elaborate, firrtl, fmt, lexer, lower, parser, resolve, schedule, types};

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let explain = args.iter().any(|a| a == "--explain-schedule");
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
            "usage: trace <file.tr | -> [--explain-schedule] [--elaborate] [--lower] \
             [--firrtl] [--fmt [--write]]"
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

    if !explain && !show_elaborate && !show_lower && !show_firrtl {
        print!("{}", ast.dump());
    }

    let (res, resolve_errors) = resolve::resolve(&ast);
    for err in &resolve_errors {
        report(&path, &src, err.span.clone(), &err.message);
    }
    if !resolve_errors.is_empty() {
        return std::process::ExitCode::FAILURE;
    }

    let (fx, effect_errors) = effects::check(&ast, &res);
    for err in &effect_errors {
        report(&path, &src, err.span.clone(), &err.message);
    }
    if !effect_errors.is_empty() {
        return std::process::ExitCode::FAILURE;
    }

    if show_elaborate {
        let (edits, elab_errors) = elaborate::plan(&ast, &res, &fx, &src);
        for err in &elab_errors {
            report(&path, &src, err.span.clone(), &err.message);
        }
        if !elab_errors.is_empty() {
            return std::process::ExitCode::FAILURE;
        }
        print!("{}", elaborate::render(&src, &edits));
        return std::process::ExitCode::SUCCESS;
    }

    let (ty, type_errors) = types::check(&ast, &res);
    for err in &type_errors {
        report(&path, &src, err.span.clone(), &err.message);
    }
    if !type_errors.is_empty() {
        return std::process::ExitCode::FAILURE;
    }

    if show_lower {
        let (lowered, lower_errors) = lower::plan(&ast, &res, &fx, &ty);
        for err in &lower_errors {
            report(&path, &src, err.span.clone(), &err.message);
        }
        if !lower_errors.is_empty() {
            return std::process::ExitCode::FAILURE;
        }
        print!("{}", lower::render(&ast, &src, &lowered));
        return std::process::ExitCode::SUCCESS;
    }

    let (sched, schedule_errors) = schedule::schedule(&ast, &res, &fx);
    for err in &schedule_errors {
        report(&path, &src, err.span.clone(), &err.message);
    }
    if !schedule_errors.is_empty() {
        return std::process::ExitCode::FAILURE;
    }
    if explain {
        print!("{}", sched.explain(&ast, &res));
    }
    if show_firrtl {
        match firrtl::emit(&ast, &res, &fx, &ty, &sched) {
            Ok(text) => print!("{text}"),
            Err(errs) => {
                for err in &errs {
                    report(&path, &src, err.span.clone(), &err.message);
                }
                return std::process::ExitCode::FAILURE;
            }
        }
    }
    std::process::ExitCode::SUCCESS
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
        let (main, hint) = split_hint(
            "state write would silently truncate bits[16] to bits[8]; use `trunc(value, 8)`",
        );
        assert_eq!(
            main,
            "state write would silently truncate bits[16] to bits[8]"
        );
        assert_eq!(hint, Some("use `trunc(value, 8)`"));
    }

    #[test]
    fn splits_the_logical_not_bits_1_hint() {
        // Regression test: this message originally read "...got bits[8]
        // (use `~` for...)" — a parenthetical, not a `"; use "` clause —
        // so `split_hint` never found it and the full message repeated
        // in both the header and the underline label (Lumi caught this
        // by eye, same double-repeat `split_hint`'s own doc comment
        // exists specifically to avoid).
        let (main, hint) = split_hint(
            "`!` needs a bits[1] operand, got bits[8]; use `~` for a bitwise complement \
             of a wider value, or compare explicitly",
        );
        assert_eq!(main, "`!` needs a bits[1] operand, got bits[8]");
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
}
