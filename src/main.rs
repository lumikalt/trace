use ariadne::{Label, Report, ReportKind, Source};
use trace::{effects, lexer, parser, resolve, types};

fn main() -> std::process::ExitCode {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: trace <file.tr>");
        return std::process::ExitCode::FAILURE;
    };
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

    print!("{}", ast.dump());

    let (res, resolve_errors) = resolve::resolve(&ast);
    for err in &resolve_errors {
        report(&path, &src, err.span.clone(), &err.message);
    }
    if !resolve_errors.is_empty() {
        return std::process::ExitCode::FAILURE;
    }

    let (_, effect_errors) = effects::check(&ast, &res);
    for err in &effect_errors {
        report(&path, &src, err.span.clone(), &err.message);
    }
    if !effect_errors.is_empty() {
        return std::process::ExitCode::FAILURE;
    }

    let (_, type_errors) = types::check(&ast, &res);
    for err in &type_errors {
        report(&path, &src, err.span.clone(), &err.message);
    }
    if !type_errors.is_empty() {
        return std::process::ExitCode::FAILURE;
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
