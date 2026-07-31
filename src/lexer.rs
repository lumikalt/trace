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

    // Structural keywords. Effect names (combines, sequences, elaborates,
    // reads, writes, chooses) and schedule-block words (urgency,
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
    #[token("input")]
    Input,
    #[token("output")]
    Output,
    #[token("inst")]
    Inst,
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
    /// A Verilog-style sized literal: `<width>'<radix?><value>` — e.g.
    /// `8'd6`, `8'hFF`, `8'b1010`, `8'o17`, or `8'6` (no radix letter,
    /// defaulting to decimal). Matched as its own token, longer than the
    /// plain `Int` alternative for the same input, so logos's longest-
    /// match rule always prefers this over reading just the width as a
    /// bare `Int` and leaving `'d6` dangling.
    #[regex(
        r"[0-9][0-9_]*'(d[0-9][0-9_]*|h[0-9A-Fa-f][0-9A-Fa-f_]*|b[01][01_]*|o[0-7][0-7_]*|[0-9][0-9_]*)"
    )]
    SizedInt,

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

/// Lex the whole source. Unrecognized characters become `LexError`s.
///
/// Lexing continues past them so one bad byte reports once, not a cascade.
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
