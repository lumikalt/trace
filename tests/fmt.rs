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
    let src = "module M {\n    reg x : bits[8] = 0\n\n    rule r {\n        x := x + 1\n    }\n}\n";
    assert_eq!(format(src), src);
}

#[test]
fn fixes_wrong_indentation() {
    let src = "module M {\nreg x : bits[8] = 0\n        rule r {\nx := x + 1\n}\n}\n";
    assert_eq!(
        format(src),
        "module M {\n    reg x : bits[8] = 0\n    rule r {\n        x := x + 1\n    }\n}\n"
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
    let src = "module M {   \n    reg x : bits[8] = 0\t\n}\n";
    let out = format(src);
    assert!(!out.lines().any(|l| l != l.trim_end()));
}

#[test]
fn no_trailing_newline_is_preserved() {
    let src = "module M {\n    reg x : bits[8] = 0\n}";
    let out = format(src);
    assert!(!out.ends_with('\n'));
}

#[test]
fn idempotent_on_every_shipped_example_except_the_known_continuation_case() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/examples");
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "tr") {
            continue;
        }
        if path.file_name().unwrap() == "arbiter.tr" {
            continue; // see known_limitation_continuation_line_before_brace below
        }
        let src = std::fs::read_to_string(&path).unwrap();
        assert_eq!(format(&src), src, "not idempotent on {path:?}");
    }
}

/// Documents the limitation named in fmt.rs's module doc comment,
/// rather than leaving it as a silent surprise: a signature that wraps
/// onto its own line before `refines`/`{` has no bracket depth to hang
/// an indent off, so the formatter renders it flush left even though
/// the source hand-indents it. Pinned here so a future change to the
/// algorithm is a deliberate decision, not an accidental regression.
#[test]
fn known_limitation_continuation_line_before_brace() {
    let src = "\
impl RoundRobin(reqs : bits[N]) : bits[clog2(N)] <combines>
    refines AnyGrant
{
    return prio(reqs)
}
";
    let out = format(src);
    assert!(out.contains("\nrefines AnyGrant\n"), "{out}");
}
