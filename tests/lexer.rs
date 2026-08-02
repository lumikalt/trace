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
    // Bare `=` does double duty: a `reg`/`out` initializer marker here,
    // equality inside an expression below — disambiguated purely by
    // parser position (`parse_state_decl` consumes it before any
    // expression parsing begins), same as `<...>` already disambiguates
    // an effect list from less-than/greater-than.
    assert_eq!(kinds("x : t = 0"), vec![Ident, Colon, Ident, Eq, Int]);
    assert_eq!(kinds("x = y"), vec![Ident, Eq, Ident]);
}

#[test]
fn comparison_and_shift_disambiguate() {
    assert_eq!(kinds("a <= b"), vec![Ident, Le, Ident]);
    assert_eq!(kinds("a < b"), vec![Ident, Lt, Ident]);
    assert_eq!(kinds("a >> 1"), vec![Ident, Shr, Int]);
    // `<>` (not-equal) is matched greedily over a bare `<` the same way
    // `>>>` beats `>>` — no explicit priority needed, logos always
    // prefers the longest token at a position.
    assert_eq!(kinds("a <> b"), vec![Ident, LtGt, Ident]);
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
fn sized_integer_literals() {
    assert_eq!(
        kinds("8'd6 8'hFF 8'b1010 8'o17 8'6 8'd1_000"),
        vec![SizedInt, SizedInt, SizedInt, SizedInt, SizedInt, SizedInt]
    );
}

#[test]
fn guard_and_fallible_call() {
    assert_eq!(
        kinds("(mode = Draining)?"),
        vec![LParen, Ident, Eq, Ident, RParen, Question]
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
fn bit_is_its_own_keyword_distinct_from_bits() {
    // `bit` (sugar for `bits[1]`) is a real lexer keyword, not a
    // `resolve.rs` BUILTINS identifier like `bits` itself — same
    // longest-match-wins guarantee already covers the prefix overlap
    // (`bits`, `bitmask`, ... all still lex as one `Ident`, not `Bit`
    // followed by leftover characters).
    assert_eq!(kinds("bit"), vec![Bit]);
    assert_eq!(kinds("bits"), vec![Ident]);
    assert_eq!(kinds("bitmask"), vec![Ident]);
}

#[test]
fn not_is_a_keyword_not_an_identifier_prefix() {
    // `not` (Verse-spelled logical negation, replacing the old symbolic
    // `!`) is a real lexer keyword — same longest-match-wins guarantee
    // as `bit`/`bits` above covers the identifier-prefix overlap
    // (`notify`, etc. still lex as one `Ident`, not `Not` followed by
    // leftover characters).
    assert_eq!(kinds("not"), vec![Not]);
    assert_eq!(kinds("notify"), vec![Ident]);
}

#[test]
fn indexed_part_select_operators_beat_a_bare_plus_or_minus() {
    // `+:`/`-:` (Verilog-style indexed part-select) are their own
    // two-character tokens, distinct from `+`/`-` followed by `:` —
    // longest-match-wins, same mechanism `:=` already relies on to beat
    // a bare `:`.
    assert_eq!(kinds("base +: 4"), vec![Ident, PlusColon, Int]);
    assert_eq!(kinds("base -: 4"), vec![Ident, MinusColon, Int]);
    // A bare `+`/`-` (no immediately-following `:`) is unaffected.
    assert_eq!(kinds("a + b"), vec![Ident, Plus, Ident]);
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
