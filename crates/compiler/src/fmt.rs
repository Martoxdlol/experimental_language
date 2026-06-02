//! `otter_fusion fmt` (`docs/23`): a conservative source formatter.
//!
//! Scope (deliberately limited to be *provably non-corrupting*): it normalizes
//! **indentation** (two spaces per nesting level, by bracket depth), strips
//! trailing whitespace, collapses runs of blank lines, and ensures a single
//! trailing newline. It does **not** rewrap lines or re-space within a line, so
//! it never moves a token across a line boundary. Every reformat is verified by
//! re-lexing the output and checking the token stream is identical to the input
//! (same kinds, same text) — so `fmt` can only ever change whitespace, never
//! code. (Full token-level spacing/wrapping is a documented follow-up.)
//!
//! The scanner is string- and comment-aware: brackets inside `"…"` strings,
//! `//` line comments, and nested `/* … */` block comments do not affect depth,
//! and lines inside a multi-line block comment are left verbatim.

use crate::lexer::lex;
use crate::span::FileId;
use crate::token::TokenKind;

const INDENT: &str = "  ";

/// Format `src`, returning the reformatted text. Pure (no I/O).
pub fn format_source(src: &str) -> String {
    // Per-line structural state, computed by one string/comment-aware scan:
    // `(bracket_depth_at_line_start, inside_block_comment_at_line_start)`.
    let line_state = scan_line_state(src);

    let mut out = String::with_capacity(src.len() + 16);
    let mut blank_run = 0usize;
    for (i, raw) in src.split('\n').enumerate() {
        // `split('\n')` yields a trailing "" for a final newline; handle EOF after.
        let (depth, in_block) = line_state.get(i).copied().unwrap_or((0, false));
        let line = raw.strip_suffix('\r').unwrap_or(raw); // tolerate CRLF

        if in_block {
            // Inside a multi-line block comment: leave the line verbatim (its
            // leading/trailing spaces may be meaningful comment art).
            out.push_str(line);
            out.push('\n');
            blank_run = 0;
            continue;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            // Collapse 2+ consecutive blank lines into a single blank line.
            blank_run += 1;
            if blank_run == 1 {
                out.push('\n');
            }
            continue;
        }
        blank_run = 0;

        // A line beginning with a closing bracket dedents one level.
        let first = trimmed.as_bytes()[0];
        let close_lead = matches!(first, b'}' | b')' | b']');
        let level = depth.saturating_sub(close_lead as usize);
        for _ in 0..level {
            out.push_str(INDENT);
        }
        out.push_str(trimmed);
        out.push('\n');
    }

    // Exactly one trailing newline (an empty file formats to empty).
    while out.ends_with("\n\n") {
        out.pop();
    }
    if out == "\n" {
        out.clear();
    }
    out
}

/// For each source line, the bracket depth at its start and whether it begins
/// inside a (nested, multi-line) block comment. A single forward scan tracks
/// string / line-comment / block-comment state so brackets in those contexts
/// are ignored.
fn scan_line_state(src: &str) -> Vec<(usize, bool)> {
    let b = src.as_bytes();
    let mut states = Vec::new();
    let mut depth: usize = 0;
    let mut block: usize = 0; // nested block-comment depth
    let mut i = 0;
    // Record the state at the start of line 0.
    states.push((depth, block > 0));
    while i < b.len() {
        if block > 0 {
            // Inside a block comment: only `/*` (deeper) and `*/` (shallower)
            // and newlines matter.
            if b[i] == b'\n' {
                i += 1;
                states.push((depth, block > 0));
                continue;
            }
            if i + 1 < b.len() && b[i] == b'/' && b[i + 1] == b'*' {
                block += 1;
                i += 2;
                continue;
            }
            if i + 1 < b.len() && b[i] == b'*' && b[i + 1] == b'/' {
                block -= 1;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        match b[i] {
            b'\n' => {
                i += 1;
                states.push((depth, false));
            }
            b'/' if i + 1 < b.len() && b[i + 1] == b'/' => {
                // Line comment: skip to (but not past) the newline.
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < b.len() && b[i + 1] == b'*' => {
                block += 1;
                i += 2;
            }
            b'"' => {
                // A single-line string (newline terminates it in the lexer).
                // Skip its contents, honouring `\` escapes, until the closing
                // quote or end of line.
                i += 1;
                while i < b.len() && b[i] != b'"' && b[i] != b'\n' {
                    if b[i] == b'\\' && i + 1 < b.len() {
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                if i < b.len() && b[i] == b'"' {
                    i += 1;
                }
            }
            b'\'' => {
                // A char literal: `'a'`, `'\n'`, `'\u{1F600}'`. Skip to the
                // closing quote on the same line, honouring escapes.
                i += 1;
                while i < b.len() && b[i] != b'\'' && b[i] != b'\n' {
                    if b[i] == b'\\' && i + 1 < b.len() {
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                if i < b.len() && b[i] == b'\'' {
                    i += 1;
                }
            }
            b'{' | b'(' | b'[' => {
                depth += 1;
                i += 1;
            }
            b'}' | b')' | b']' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            _ => i += 1,
        }
    }
    states
}

/// The safety invariant: `fmt` may only change whitespace, never code. Re-lex
/// both texts and require an identical token stream — same kinds and same source
/// text per token. Returns `true` when formatting is safe to apply.
pub fn token_stream_preserved(before: &str, after: &str) -> bool {
    let (ta, _) = lex(before, FileId(0));
    let (tb, _) = lex(after, FileId(0));
    if ta.len() != tb.len() {
        return false;
    }
    ta.iter()
        .zip(tb.iter())
        .all(|(x, y)| x.kind == y.kind && tok_text(before, x) == tok_text(after, y))
}

fn tok_text<'a>(src: &'a str, t: &crate::token::Token) -> &'a str {
    let r = t.span.range();
    src.get(r).unwrap_or("")
}

/// Whether `kind` is a doc-comment token (whose *text* `fmt` must not change —
/// it keeps comment content verbatim, only restyling surrounding indentation).
#[allow(dead_code)]
fn is_trivia(kind: TokenKind) -> bool {
    matches!(kind, TokenKind::DocOuter | TokenKind::DocInner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reindents_by_bracket_depth() {
        let src = "function f() {\nvar x = 1;\nif x > 0 {\nreturn;\n}\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var x = 1;\n  if x > 0 {\n    return;\n  }\n}\n"
        );
        // Idempotent.
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn strips_trailing_ws_and_collapses_blanks() {
        let src = "function f() {  \n\n\n\n  var x = 1;   \n}\n";
        let out = format_source(src);
        assert_eq!(out, "function f() {\n\n  var x = 1;\n}\n");
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn preserves_string_contents() {
        // Braces and double-spaces *inside* a string must be untouched, and a
        // `}` inside a string must not affect indentation.
        let src = "function f() {\nvar s = \"a  }  {  b\";\n}\n";
        let out = format_source(src);
        assert!(out.contains("\"a  }  {  b\""), "string mangled: {out:?}");
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn leaves_block_comment_interior_verbatim() {
        let src = "function f() {\n/* a\n    b\n  c */\nvar x = 1;\n}\n";
        let out = format_source(src);
        // The comment's interior lines keep their original leading spaces.
        assert!(
            out.contains("/* a\n    b\n  c */"),
            "block comment changed: {out:?}"
        );
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn trailing_newline_normalized() {
        assert_eq!(format_source("var x = 1;"), "var x = 1;\n");
        assert_eq!(format_source("var x = 1;\n\n\n"), "var x = 1;\n");
        assert_eq!(format_source(""), "");
    }
}
