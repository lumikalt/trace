use trace::{fmt, lexer};

fn format(src: &str) -> String {
    let (tokens, errors) = lexer::lex(src);
    assert!(errors.is_empty(), "lex errors: {errors:?}");
    fmt::format(src, &tokens)
}

#[test]
fn reindents_nested_blocks() {
    let src = "module M {\nrule r {\nif x = 1 {\ny := 1\n}\n}\n}\n";
    assert_eq!(
        format(src),
        "module M {\n    rule r {\n        if x = 1 {\n            y := 1\n        }\n    }\n}\n"
    );
}

#[test]
fn already_formatted_input_is_unchanged() {
    let src = "module M {\n    reg x : [8] = 0\n\n    rule r {\n        x := x + 1\n    }\n}\n";
    assert_eq!(format(src), src);
}

#[test]
fn fixes_wrong_indentation() {
    let src = "module M {\nreg x : [8] = 0\n        rule r {\nx := x + 1\n}\n}\n";
    assert_eq!(
        format(src),
        "module M {\n    reg x : [8] = 0\n    rule r {\n        x := x + 1\n    }\n}\n"
    );
}

#[test]
fn comments_and_blank_lines_survive_and_inherit_surrounding_indent() {
    let src = "module M {\n    -- a top-level comment\n\n    rule r {\n-- nested comment, wrong indent in the source\n\n        tick\n    }\n}\n";
    let out = format(src);
    assert!(out.contains("-- a top-level comment"));
    assert!(out.contains("-- nested comment, wrong indent in the source"));
    // The nested comment inherits the rule body's depth (one level in),
    // not its own (wrong) source indentation.
    assert!(out.contains("        -- nested comment, wrong indent in the source"));
    // Blank lines stay blank, not indented.
    assert!(!out.lines().any(|l| l.trim().is_empty() && !l.is_empty()));
}

#[test]
fn trailing_whitespace_is_trimmed() {
    let src = "module M {   \n    reg x : [8] = 0\t\n}\n";
    let out = format(src);
    assert!(!out.lines().any(|l| l != l.trim_end()));
}

#[test]
fn no_trailing_newline_is_preserved() {
    let src = "module M {\n    reg x : [8] = 0\n}";
    let out = format(src);
    assert!(!out.ends_with('\n'));
}

#[test]
fn idempotent_on_every_shipped_example() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/examples");
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "tr") {
            continue;
        }
        let src = std::fs::read_to_string(&path).unwrap();
        assert_eq!(format(&src), src, "not idempotent on {path:?}");
    }
}

/// A multiline `impl` signature's `refines` line — the one construct
/// the parser's grammar (`parse_fn`'s `skip_newlines()` before
/// `Refines`) allows to continue a signature outside any bracket — gets
/// one extra indent level, matching DESIGN.md's own `RoundRobin`
/// example (`examples/arbiter.tr`) exactly: one level deeper than
/// `impl`'s own line, with `{` back at the signature's own depth. Used
/// to render flush left instead (fmt.rs's own module doc comment
/// documents why plain brace-counting alone can't see this).
#[test]
fn a_multiline_impl_signatures_refines_line_gets_one_extra_indent() {
    let src = "\
impl RoundRobin(reqs : [N]) : [clog2(N)] <combines>
    refines AnyGrant
{
    return prio(reqs)
}
";
    assert_eq!(format(src), src);
}

/// Same as above, one level deeper (`impl` itself nested inside a
/// module) — confirms `refines`'s extra indent is relative to whatever
/// depth its own signature line sits at, not hardcoded to top level.
#[test]
fn a_nested_multiline_impl_signatures_refines_line_indents_one_past_its_own_depth() {
    let src = "\
module M {
    spec AnyGrant(reqs : [N]) : [clog2(N)] <combines, chooses, fails> {
        return 0
    }

    impl RoundRobin(reqs : [N]) : [clog2(N)] <combines>
        refines AnyGrant
    {
        return prio(reqs)
    }
}
";
    assert_eq!(format(src), src);
}
