use logos::Logos;

/// Byte-offset span into the source text.
pub type Span = std::ops::Range<usize>;

#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
#[logos(skip r"[ \t\r]+")]
#[logos(skip(r"--[^\n]*", allow_greedy = true))]
pub enum TokenKind {
    // Statements are newline-terminated; the parser collapses runs.
    #[token("\n")]
    Newline,

    // Structural keywords. Effect names (converges, suspends, allocates,
    // reads, writes, choice) and schedule-block words (urgency,
    // conflict_free) stay contextual identifiers — the keyword set only
    // holds words that open or shape a construct.
    #[token("module")]
    Module,
    #[token("rule")]
    Rule,
    #[token("reg")]
    Reg,
    #[token("mem")]
    Mem,
    #[token("fifo")]
    Fifo,
    #[token("spec")]
    Spec,
    #[token("impl")]
    Impl,
    #[token("refines")]
    Refines,
    #[token("schedule")]
    Schedule,
    #[token("tick")]
    Tick,
    #[token("sync")]
    Sync,
    #[token("race")]
    Race,
    #[token("spawn")]
    Spawn,
    #[token("return")]
    Return,
    #[token("if")]
    If,
    #[token("else")]
    Else,
    #[token("while")]
    While,
    #[token("let")]
    Let,

    #[token("_", priority = 100)]
    Underscore,
    #[regex(r"[A-Za-z_][A-Za-z0-9_]*")]
    Ident,
    #[regex(r"0x[0-9A-Fa-f][0-9A-Fa-f_]*|0b[01][01_]*|[0-9][0-9_]*")]
    Int,

    #[token(":=")]
    ColonEq,
    #[token(":")]
    Colon,
    #[token("==")]
    EqEq,
    #[token("=")]
    Eq,
    #[token("!=")]
    BangEq,
    #[token("<=")]
    Le,
    #[token(">=")]
    Ge,
    #[token("<<")]
    Shl,
    #[token(">>")]
    Shr,
    #[token("<")]
    Lt,
    #[token(">")]
    Gt,
    #[token("..")]
    DotDot,
    #[token(".")]
    Dot,
    #[token("?")]
    Question,
    #[token("|")]
    Pipe,
    #[token("^")]
    Caret,
    #[token("&")]
    Amp,
    #[token("+")]
    Plus,
    #[token("-")]
    Minus,
    #[token("*")]
    Star,
    #[token("/")]
    Slash,
    #[token("%")]
    Percent,
    #[token("~")]
    Tilde,
    #[token("!")]
    Bang,
    #[token(",")]
    Comma,
    #[token(";")]
    Semi,
    #[token("{")]
    LBrace,
    #[token("}")]
    RBrace,
    #[token("(")]
    LParen,
    #[token(")")]
    RParen,
    #[token("[")]
    LBracket,
    #[token("]")]
    RBracket,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexError {
    pub span: Span,
}

/// Lex the whole source. Unrecognized characters become `LexError`s;
/// lexing continues past them so one bad byte reports once, not a cascade.
pub fn lex(src: &str) -> (Vec<Token>, Vec<LexError>) {
    let mut tokens = Vec::new();
    let mut errors: Vec<LexError> = Vec::new();
    for (result, span) in TokenKind::lexer(src).spanned() {
        match result {
            Ok(kind) => tokens.push(Token { kind, span }),
            Err(()) => match errors.last_mut() {
                // Merge adjacent bad bytes into one span.
                Some(last) if last.span.end == span.start => last.span.end = span.end,
                _ => errors.push(LexError { span }),
            },
        }
    }
    (tokens, errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use TokenKind::*;

    fn kinds(src: &str) -> Vec<TokenKind> {
        let (tokens, errors) = lex(src);
        assert!(errors.is_empty(), "unexpected lex errors: {errors:?}");
        tokens.into_iter().map(|t| t.kind).collect()
    }

    #[test]
    fn fifo_bridge_module() {
        let src = "\
module FifoBridge {
    fifo input : bits[8]

    rule transfer {
        x := input.Deq[]      -- fails when input is empty
    }
}
";
        assert_eq!(
            kinds(src),
            vec![
                Module, Ident, LBrace, Newline, // module FifoBridge {
                Fifo, Ident, Colon, Ident, LBracket, Int, RBracket, Newline, // fifo decl
                Newline, // blank line
                Rule, Ident, LBrace, Newline, // rule transfer {
                Ident, ColonEq, Ident, Dot, Ident, LBracket, RBracket, Newline, // x := input.Deq[]
                RBrace, Newline, // }
                RBrace, Newline, // }
            ]
        );
    }

    #[test]
    fn assign_family_disambiguates() {
        assert_eq!(kinds("x := y"), vec![Ident, ColonEq, Ident]);
        assert_eq!(kinds("x : t = 0"), vec![Ident, Colon, Ident, Eq, Int]);
        assert_eq!(kinds("x == y"), vec![Ident, EqEq, Ident]);
    }

    #[test]
    fn comparison_and_shift_disambiguate() {
        assert_eq!(kinds("a <= b"), vec![Ident, Le, Ident]);
        assert_eq!(kinds("a < b"), vec![Ident, Lt, Ident]);
        assert_eq!(kinds("a >> 1"), vec![Ident, Shr, Int]);
        // Effect brackets are plain Lt/Gt; the parser owns that grammar.
        assert_eq!(kinds("<converges>"), vec![Lt, Ident, Gt]);
    }

    #[test]
    fn ranges_vs_field_access() {
        assert_eq!(kinds("0..7"), vec![Int, DotDot, Int]);
        assert_eq!(kinds("h1.result"), vec![Ident, Dot, Ident]);
    }

    #[test]
    fn comment_runs_to_eol_but_keeps_newline() {
        assert_eq!(kinds("a -- b + c\nd"), vec![Ident, Newline, Ident]);
    }

    #[test]
    fn wildcard_vs_ident() {
        assert_eq!(kinds("_"), vec![Underscore]);
        assert_eq!(kinds("_save"), vec![Ident]);
        assert_eq!(kinds("Fifo(_, x)"), vec![Ident, LParen, Underscore, Comma, Ident, RParen]);
    }

    #[test]
    fn integer_literals() {
        assert_eq!(kinds("42 0xFF 0b1010 1_000"), vec![Int, Int, Int, Int]);
    }

    #[test]
    fn guard_and_fallible_call() {
        assert_eq!(
            kinds("(mode == Draining)?"),
            vec![LParen, Ident, EqEq, Ident, RParen, Question]
        );
        assert_eq!(
            kinds("output.Enq[x]"),
            vec![Ident, Dot, Ident, LBracket, Ident, RBracket]
        );
    }

    #[test]
    fn keywords_are_not_idents() {
        assert_eq!(kinds("tick"), vec![Tick]);
        assert_eq!(kinds("ticker"), vec![Ident]); // longest match wins
        assert_eq!(kinds("spawn ReadBank"), vec![Spawn, Ident]);
    }

    #[test]
    fn bad_bytes_merge_into_one_error() {
        let (tokens, errors) = lex("a @@ b");
        assert_eq!(
            tokens.iter().map(|t| t.kind).collect::<Vec<_>>(),
            vec![Ident, Ident]
        );
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].span, 2..4);
    }
}
