//! A simple formatter: fixes each line's leading whitespace to match
//! brace/paren/bracket nesting depth. Nothing else about a line
//! changes — inline spacing, trailing comments, and blank-line runs
//! are all left exactly as written.
//!
//! This is deliberately not an AST pretty-printer. The lexer treats
//! `--` comments as trivia and never tokenizes them (see lexer.rs), so
//! there is no way to reattach a comment to the right place after
//! re-emitting from the AST — a pretty-printer would silently delete
//! every comment in the file. Reindenting from the token stream while
//! keeping each line's original text intact sidesteps that entirely:
//! comments are never seen, so they can never be lost.
//!
//! Known limitation, accepted rather than special-cased: a line that
//! continues a statement without opening a bracket — DESIGN.md's
//! multiline-signature style, `impl F(...) : ty <combines>\n    refines
//! Spec\n{` — has no bracket depth to hang an indent off, so it renders
//! flush left instead of hand-indented. Fixing that needs real
//! statement awareness, not brace counting; out of scope for a
//! "simple" formatter (see tests/fmt.rs).

use crate::lexer::{Token, TokenKind};

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

pub fn format(src: &str, tokens: &[Token]) -> String {
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
