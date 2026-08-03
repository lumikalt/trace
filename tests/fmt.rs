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

/// The reported bug: a multi-line block's closing `}` gets merged onto
/// the end of its last content line (a deleted newline, an editor
/// merge gone wrong) — plain brace-counting alone leaves it there,
/// since it never rearranges tokens across lines, only reindents
/// existing ones. `split_stray_closers` (fmt.rs) gives it back its own
/// line before the normal reindent pass runs.
#[test]
fn a_closing_brace_merged_onto_its_last_content_line_gets_split_out() {
    let src = "module M {\n    rule r {\n        y := 1    }\n}\n";
    assert_eq!(
        format(src),
        "module M {\n    rule r {\n        y := 1\n    }\n}\n"
    );
}

/// Same bug, `)`/`]` instead of `}` — the rule isn't brace-specific.
#[test]
fn a_stray_closing_paren_or_bracket_also_gets_split_out() {
    let src = "module M {\n    reg x : [8] = f(\n        1 + 2)\n}\n";
    assert_eq!(
        format(src),
        "module M {\n    reg x : [8] = f(\n        1 + 2\n    )\n}\n"
    );
}

/// A deliberate single-line block (opener AND closer on the same
/// source line) must NOT be split apart — only a closer whose OPENER
/// is on an earlier line is ever a formatting slip; this is a genuine
/// style choice used throughout examples/ (`if x = 1 { y := 1 }`).
#[test]
fn a_genuine_single_line_block_is_left_alone() {
    let src = "module M {\n    rule r {\n        if x = 1 { y := 1 }\n    }\n}\n";
    assert_eq!(format(src), src);
}

/// A run of stacked closers (`}))`) that's ALREADY alone on its own
/// line must stay stacked together, not get blown apart into one
/// bracket per line — each one being "first on its line" (after the
/// one before it) is exactly what makes this idiom legible.
#[test]
fn an_already_correct_stacked_closer_line_is_left_alone() {
    let src = "module M {\n    reg x : [8] = f(g(\n            1\n        )))\n}\n";
    assert_eq!(format(src), src);
}

/// The same stacked-closer idiom, but reached via a split: the merged
/// line's whole trailing closer run moves together as one unit, not
/// one bracket at a time.
#[test]
fn a_stray_stacked_closer_run_splits_out_as_one_unit() {
    let src = "module M {\n    reg x : [8] = f(g(\n            1)))\n}\n";
    assert_eq!(
        format(src),
        "module M {\n    reg x : [8] = f(g(\n            1\n        )))\n}\n"
    );
}

/// `format` is itself a two-pass pipeline now (split stray closers,
/// re-lex, reindent) — this is the property that pipeline depends on:
/// formatting already-formatted output is a no-op, for every case above
/// where the input actually needed a split. Not implied by any single
/// test above; each of those only checks the FIRST pass's output.
#[test]
fn formatting_is_idempotent_on_every_split_case_above() {
    for src in [
        "module M {\n    rule r {\n        y := 1    }\n}\n",
        "module M {\n    reg x : [8] = f(\n        1 + 2)\n}\n",
        "module M {\n    reg x : [8] = f(g(\n            1)))\n}\n",
    ] {
        let once = format(src);
        assert_eq!(format(&once), once, "not idempotent starting from {src:?}");
    }
}

/// A trailing `--` comment survives a split unmoved, riding along on
/// whichever side of the new line break it started on — here that's
/// the closer's own new line, since the comment came after it. This
/// composes fine with `split_stray_closers` precisely because a
/// comment can only ever trail a real token on a line, never precede
/// one (see fmt.rs's own module doc comment): the split point is
/// always inserted before a real token, so it can never land in the
/// middle of a comment's span.
#[test]
fn a_trailing_comment_after_a_stray_closer_stays_with_it() {
    let src = "module M {\n    rule r {\n        y := 1    } -- done\n}\n";
    assert_eq!(
        format(src),
        "module M {\n    rule r {\n        y := 1\n    } -- done\n}\n"
    );
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
