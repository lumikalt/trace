use ariadne::{Label, Report, ReportKind, Source};
use trace::{effects, lexer, lower, parser, resolve, schedule, types};

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let explain = args.iter().any(|a| a == "--explain-schedule");
    let show_lower = args.iter().any(|a| a == "--lower");
    let Some(path) = args.iter().find(|a| !a.starts_with("--")) else {
        eprintln!("usage: trace <file.tr> [--explain-schedule] [--lower]");
        return std::process::ExitCode::FAILURE;
    };
    let path = path.clone();
    let src = match std::fs::read_to_string(&path) {
        Ok(src) => src,
        Err(e) => {
            eprintln!("trace: cannot read {path}: {e}");
            return std::process::ExitCode::FAILURE;
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

    if !explain && !show_lower {
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
    std::process::ExitCode::SUCCESS
}

fn report(path: &str, src: &str, span: lexer::Span, message: &str) {
    Report::build(ReportKind::Error, (path, span.clone()))
        .with_message(message)
        .with_label(Label::new((path, span)).with_message(message))
        .finish()
        .eprint((path, Source::from(src)))
        .expect("failed to print diagnostic");
}
