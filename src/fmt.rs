//! A simple formatter: fixes each line's leading whitespace to match
//! brace/paren/bracket nesting depth. Nothing else about a line
//! changes — inline spacing, trailing comments, and blank-line runs
//! are all left exactly as written. The one exception is `split_stray_
//! closers` below, which can insert a bare newline before a closing
//! bracket; everything else is unchanged reindenting.
//!
//! This is deliberately not an AST pretty-printer. The lexer treats
//! `--` comments as trivia and never tokenizes them (see lexer.rs), so
//! there is no way to reattach a comment to the right place after
//! re-emitting from the AST — a pretty-printer would silently delete
//! every comment in the file. Reindenting from the token stream while
//! keeping each line's original text intact sidesteps that entirely:
//! comments are never seen, so they can never be lost. `split_stray_
//! closers` keeps that invariant too: since a `--` comment always runs
//! to end of line and is never tokenized, a REAL token (a closing
//! bracket included) can never appear after one on the same line in
//! valid source — a `}` "inside" a comment would leave the brace count
//! unbalanced, a parse error, not something this formatter ever sees.
//! So deciding where to insert a newline never has to reason about
//! comment positions at all: it only ever inserts one immediately
//! before a real token, never after or across one.
//!
//! One special case beyond plain brace counting: `impl F(...) : ty
//! <combines>\n    refines Spec\n{` (DESIGN.md's multiline-signature
//! style) — `refines` is the one construct the parser's own grammar
//! (`parser.rs`'s `parse_fn`) allows to continue a signature on its own
//! line, outside any bracket, so a `Refines` token starting a line gets
//! one extra indent level. Nothing else needed real statement
//! awareness: every other multiline-signature gap is either inside an
//! open paren/bracket (already handled by plain depth counting) or
//! directly before `{`, which stays at the signature's own depth by
//! design (see tests/fmt.rs).

use crate::lexer::{self, Token, TokenKind};

const INDENT_UNIT: &str = "    ";

/// Byte offset where each (0-indexed) line begins, `src.split('\n')`
/// numbering (so a trailing `\n` yields one extra, empty final line —
/// matching how the line-rendering pass below reconstructs the file).
fn line_starts(src: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for (i, b) in src.bytes().enumerate() {
        if b == b'\n' {
            starts.push(i + 1);
        }
    }
    starts
}

fn line_of(starts: &[usize], offset: usize) -> usize {
    starts.partition_point(|&s| s <= offset) - 1
}

/// A closing bracket whose matching opener sits on an EARLIER line (so
/// the block was already, unambiguously, a multi-line one — never a
/// case of "this reads fine as one line," which is the situation plain
/// brace-counting alone must never second-guess) but which itself isn't
/// alone on its own line gets pushed onto a fresh line: this is always
/// a formatting slip (a deleted newline, a merge gone wrong), never a
/// deliberate style choice, since a deliberate single-line block has
/// its opener on the SAME line as its closer instead. A closing bracket
/// that's already the first real token on its line is left alone even
/// when others share that line after it (an `if {...} else {...}`'s
/// first `}`, immediately followed by ` else {`) — and a RUN of several
/// closers stacked together (`}))`) splits together, as one unit, not
/// apart from each other: after the run's first member gets its own
/// line, every closer immediately after it is once again "first on its
/// (now virtual) line," so the same rule leaves the rest of the run
/// untouched.
///
/// Returns the source with a `\n` inserted immediately before each such
/// closer — nothing else about the text changes here; `format`'s own
/// reindenting pass (re-lexing this returned text) does the rest,
/// including trimming whatever trailing whitespace used to precede the
/// bracket on its old line.
fn split_stray_closers(src: &str, tokens: &[Token]) -> String {
    let starts = line_starts(src);
    let mut open_stack: Vec<usize> = Vec::new();
    let mut splits: Vec<usize> = Vec::new();
    let mut seen_non_closer = false;
    for tok in tokens {
        match tok.kind {
            TokenKind::Newline => seen_non_closer = false,
            TokenKind::LBrace | TokenKind::LParen | TokenKind::LBracket => {
                open_stack.push(line_of(&starts, tok.span.start));
                seen_non_closer = true;
            }
            TokenKind::RBrace | TokenKind::RParen | TokenKind::RBracket => {
                let opener_line = open_stack.pop();
                let this_line = line_of(&starts, tok.span.start);
                if seen_non_closer && opener_line.is_some_and(|l| l != this_line) {
                    splits.push(tok.span.start);
                    seen_non_closer = false; // now first on its own (new) line
                }
                // A closer never itself sets `seen_non_closer`, whether
                // split or not — that's what lets a stacked run (`}))`)
                // either split together or stay together as one unit.
            }
            _ => seen_non_closer = true,
        }
    }
    if splits.is_empty() {
        return src.to_string();
    }
    let mut out = String::with_capacity(src.len() + splits.len());
    let mut last = 0;
    for offset in splits {
        out.push_str(&src[last..offset]);
        out.push('\n');
        last = offset;
    }
    out.push_str(&src[last..]);
    out
}

pub fn format(src: &str, tokens: &[Token]) -> String {
    let split_src = split_stray_closers(src, tokens);
    let (src, tokens) = if split_src == src {
        (src.to_string(), tokens.to_vec())
    } else {
        let (tokens, _lex_errors) = lexer::lex(&split_src);
        // Inserting a newline strictly between two existing tokens can
        // never introduce a lex error — see this module's doc comment on
        // why a real token can never sit inside a comment's span — so
        // `_lex_errors` is unconditionally empty here; not asserted, to
        // keep this a plain fallback rather than a panic on a claim that
        // (if ever wrong) would only cost a missed reindent, not a crash.
        (split_src, tokens)
    };
    let src = src.as_str();
    let tokens = tokens.as_slice();
    let starts = line_starts(src);
    let n_lines = starts.len();

    let mut has_token = vec![false; n_lines];
    let mut line_indent = vec![0i32; n_lines];
    let mut depth_after = vec![0i32; n_lines];

    let mut depth: i32 = 0;
    for tok in tokens {
        let line = line_of(&starts, tok.span.start);
        if tok.kind == TokenKind::Newline {
            depth_after[line] = depth;
            continue;
        }
        if !has_token[line] {
            has_token[line] = true;
            let mut d = depth;
            if matches!(
                tok.kind,
                TokenKind::RBrace | TokenKind::RParen | TokenKind::RBracket
            ) {
                d -= 1;
            } else if tok.kind == TokenKind::Refines {
                // `impl Name(...) : ty <effects>\n    refines Spec\n{` --
                // the parser's own `skip_newlines()` before `refines`
                // (parser.rs's `parse_fn`) makes this the one place a
                // continuation line carries real content at the
                // signature's own bracket depth. Every OTHER multiline
                // signature gap is either inside an open paren/bracket
                // (already handled by the ordinary depth count above) or
                // directly before `{`, which deliberately stays at the
                // signature's own depth. One extra level marks `refines`
                // as continuing the line above it, not a sibling
                // statement -- DESIGN.md's own `RoundRobin` example.
                d += 1;
            }
            line_indent[line] = d.max(0);
        }
        match tok.kind {
            TokenKind::LBrace | TokenKind::LParen | TokenKind::LBracket => depth += 1,
            TokenKind::RBrace | TokenKind::RParen | TokenKind::RBracket => depth -= 1,
            _ => {}
        }
    }
    // A blank or comment-only line has no tokens of its own (the lexer
    // never emits one for a comment); it inherits whatever depth was
    // in effect when the previous line ended.
    for i in 0..n_lines {
        if !has_token[i] && i > 0 {
            line_indent[i] = depth_after[i - 1].max(0);
        }
    }

    let lines: Vec<&str> = src.split('\n').collect();
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            out.push_str(&INDENT_UNIT.repeat(line_indent[i] as usize));
            out.push_str(trimmed);
        }
        if i + 1 < lines.len() {
            out.push('\n');
        }
    }
    if src.ends_with('\n') && !out.ends_with('\n') {
        out.push('\n');
    }
    out
}
