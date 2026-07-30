use ariadne::{Label, Report, ReportKind, Source};
use trace::lexer;

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

    let (tokens, errors) = lexer::lex(&src);

    for tok in &tokens {
        let text = &src[tok.span.clone()];
        let shown = if tok.kind == lexer::TokenKind::Newline {
            "\\n"
        } else {
            text
        };
        println!(
            "{:>4}..{:<4} {:?} {shown}",
            tok.span.start, tok.span.end, tok.kind
        );
    }

    for err in &errors {
        Report::build(ReportKind::Error, (path.as_str(), err.span.clone()))
            .with_message("unrecognized character(s)")
            .with_label(
                Label::new((path.as_str(), err.span.clone())).with_message("not part of any token"),
            )
            .finish()
            .eprint((path.as_str(), Source::from(src.as_str())))
            .expect("failed to print diagnostic");
    }

    if errors.is_empty() {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}
