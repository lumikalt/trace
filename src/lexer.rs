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
    // mutually_exclusive, conflict_free) stay contextual identifiers —
    // the keyword set only holds words that open or shape a construct.
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
    #[token("in")]
    Input,
    #[token("out")]
    Output,
    /// `io name : ty` — a structural, bidirectional port (lowers to
    /// FIRRTL's `Analog` type + `attach`). Unlike `in`/`out`, never
    /// readable or writable from a rule body — see `attach`, its only
    /// legal use.
    #[token("io")]
    Io,
    /// `attach a, b` — wires two `io` ports (a module's own, or an
    /// instance's) together. A structural item, not a rule statement:
    /// mirrors FIRRTL's own `attach`, an unconditional net connection
    /// with no clock/cycle semantics.
    #[token("attach")]
    Attach,
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
    #[token("struct")]
    Struct,
    #[token("tick")]
    Tick,
    #[token("sync")]
    Sync,
    #[token("race")]
    Race,
    /// Verse's failure-discharging fallback operator (`08_failure`): `A or
    /// B or C` tries each alternative in order and, unlike every other
    /// binary operator, is never a plain value expression itself — always
    /// reserved, no ident-fallback (matching `tick`, not `sync`/`race`).
    #[token("or")]
    Or,
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
    /// Verse-spelled prefix logical negation, replacing the symbolic `!`
    /// this project used before aligning operator spelling with Verse
    /// (see DESIGN.md's "Expression surface"). Purely a spelling change —
    /// same `UnOp::Not` semantics (requires a `[1]` operand, distinct
    /// from bitwise `~`) as before.
    #[token("not")]
    Not,
    /// Verse spells an absent optional value `false` (tying into its
    /// logic-programming failure model) rather than a dedicated `none`
    /// keyword — trace follows that spelling. Deliberately NOT a general
    /// `[1]` boolean literal (no `true` counterpart either): `?[1]`
    /// would make `opt := false` ambiguous between "absent" and
    /// "present, holding 0" if `false` doubled as a plain zero. Only
    /// valid where a `?T` value is expected (`types.rs`'s `Ty::AbsentLit`
    /// sentinel enforces this contextually).
    #[token("false")]
    False,
    /// Explicit one-layer "present" constructor for `?T` (`Expr::Optional`,
    /// ast.rs) — the way to build a `??T` whose two `valid` bits differ
    /// (`Some(None)`), which a bare value or `false` can't do (both
    /// coerce through every remaining `?` layer at once).
    #[token("optional")]
    Optional,
    /// `logic <expr>` (`Expr::Logic`, ast.rs) — converts a fallible
    /// expression's success into a plain `bits[1]` value without
    /// gating the enclosing rule. A real keyword, not an identifier
    /// resolved to a builtin def the way `prio`/`trunc`/`pack` still
    /// are: unlike those, `logic` is a prefix OPERATOR (no parens, no
    /// comma-separated args), matching `not`/`optional`/`spawn`'s own
    /// spelling instead of a function call's.
    #[token("logic")]
    Logic,

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
    /// Verse-spelled equality (`=`, replacing the old `==`) — also, at
    /// declaration level, the `reg`/`out` initializer marker (`reg a :
    /// bits[8] = 0`); the two uses never collide since the initializer's
    /// `=` is consumed by `parse_state_decl` before any expression (and
    /// so before the Pratt loop's own infix dispatch) ever sees a token.
    #[token("=")]
    Eq,
    /// Verse-spelled not-equal (`<>`, replacing the old `!=`). Matched
    /// before `<` the same way `>>>` beats `>>` — logos always prefers
    /// the longest token at a given position, so this needs no explicit
    /// priority annotation.
    #[token("<>")]
    LtGt,
    #[token("<=")]
    Le,
    #[token(">=")]
    Ge,
    #[token("<<")]
    Shl,
    /// Arithmetic (sign-extending) right shift, distinct from `Shr`'s
    /// logical (zero-filling) right shift — matched before `>>` since
    /// logos's own longest-match rule always prefers a 3-character
    /// token over a 2-character prefix of it.
    #[token(">>>")]
    AShr,
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
    /// Verilog-style indexed part-select (`x[base +: width]`): `base` may
    /// be a runtime expression, `width` must be a compile-time constant.
    /// Longest-match beats a bare `+` the same way `:=` already beats a
    /// bare `:`, so this needs no special handling to disambiguate from
    /// `a + b` followed by `:` in some other context.
    #[token("+:")]
    PlusColon,
    /// The descending mirror of `+:` (`x[base -: width]`).
    #[token("-:")]
    MinusColon,
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
