use trace::lexer::{TokenKind, TokenKind::*, lex};

fn kinds(src: &str) -> Vec<TokenKind> {
    let (tokens, errors) = lex(src);
    assert!(errors.is_empty(), "unexpected lex errors: {errors:?}");
    tokens.into_iter().map(|t| t.kind).collect()
}

#[test]
fn fifo_bridge_module() {
    let src = "\
module FifoBridge {
    fifo buf : bits[8]

    rule transfer {
        x := buf.Deq[]      -- fails when buf is empty
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
            Ident, ColonEq, Ident, Dot, Ident, LBracket, RBracket, Newline, // x := buf.Deq[]
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
    assert_eq!(kinds("<combines>"), vec![Lt, Ident, Gt]);
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
    assert_eq!(
        kinds("Fifo(_, x)"),
        vec![Ident, LParen, Underscore, Comma, Ident, RParen]
    );
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
        kinds("buf.Enq[x]"),
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
