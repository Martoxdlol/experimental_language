//! `otter_fusion fmt` (`docs/23`): a conservative source formatter.
//!
//! Scope (deliberately limited to be *provably non-corrupting*): it normalizes
//! **indentation** (two spaces per nesting level, by bracket depth), common
//! intra-line token spacing, trailing whitespace, blank-line runs, conservative
//! line wrapping at token boundaries, and a single trailing newline.
//! Every reformat is verified by re-lexing the output and checking parser tokens
//! plus ordinary comment trivia are identical — so `fmt` can only ever change
//! whitespace, never code or comments.
//!
//! The scanner is string- and comment-aware: brackets inside `"…"` strings,
//! `//` line comments, and nested `/* … */` block comments do not affect depth.
//! Multi-line block-comment interiors are left verbatim, while code before an
//! opening `/*` or after a closing `*/` boundary can still be spaced safely.

use crate::lexer::{lex, lex_ordinary_comments};
use crate::span::FileId;
use crate::token::{Keyword, Token, TokenKind};

const INDENT: &str = "  ";
const MAX_LINE_LENGTH: usize = 100;

/// Format `src`, returning the reformatted text. Pure (no I/O).
pub fn format_source(src: &str) -> String {
    // Per-line structural state, computed by one string/comment-aware scan.
    let line_state = scan_line_state(src);

    let mut out = String::with_capacity(src.len() + 16);
    let mut blank_run = 0usize;
    for (i, raw) in src.split('\n').enumerate() {
        // `split('\n')` yields a trailing "" for a final newline; handle EOF after.
        let state = line_state.get(i).copied().unwrap_or_default();
        let line = raw.strip_suffix('\r').unwrap_or(raw); // tolerate CRLF

        if state.block_depth > 0 {
            // Inside a multi-line block comment: leave the comment prefix
            // verbatim (its leading/trailing spaces may be meaningful comment
            // art), but normalize code that follows the boundary `*/`.
            out.push_str(&normalize_block_comment_boundary_tail(
                line,
                state.block_depth,
            ));
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

        // A line beginning with a closing delimiter dedents one level.
        let first = trimmed.as_bytes()[0];
        let close_lead =
            matches!(first, b'}' | b')' | b']') || (first == b'>' && state.generic_angle_depth > 0);
        let level = (state.bracket_depth
            + state.generic_angle_depth
            + state.type_alias_union_depth
            + state.var_type_union_depth
            + state.parameter_type_union_depth
            + state.record_field_type_union_depth
            + state.generic_bound_depth
            + state.function_return_union_depth
            + state.arrow_closure_return_union_depth
            + state.interface_bound_depth
            + state.var_initializer_depth
            + state.return_expr_depth
            + state.break_expr_depth
            + state.await_spawn_expr_depth
            + state.assignment_expr_depth
            + state.for_iterator_depth
            + state.while_condition_depth
            + state.if_condition_depth
            + state.match_scrutinee_depth
            + state.match_guard_depth
            + state.match_arm_body_depth
            + state.arrow_closure_body_depth
            + state.test_decl_header_depth
            + state.type_alias_decl_header_depth
            + state.interface_decl_header_depth
            + state.struct_decl_header_depth
            + state.function_decl_header_depth
            + state.module_decl_header_depth
            + state.extern_type_depth
            + state.extern_var_depth
            + state.extern_function_depth
            + state.import_path_depth
            + state.cast_chain_depth
            + state.method_chain_depth
            + state.logical_chain_depth
            + state.comparison_expr_depth
            + state.additive_chain_depth
            + state.multiplicative_chain_depth
            + state.shift_chain_depth
            + state.bitwise_and_chain_depth
            + state.bitwise_xor_chain_depth
            + state.bitwise_or_chain_depth)
            .saturating_sub(close_lead as usize);
        let base_indent = INDENT.repeat(level);
        out.push_str(&base_indent);
        let normalized = normalize_intra_line_spacing(trimmed, state);
        out.push_str(&wrap_long_line(&normalized, &base_indent, state));
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

/// Structural state at the start of one source line.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct LineState {
    bracket_depth: usize,
    generic_angle_depth: usize,
    type_alias_union_depth: usize,
    var_type_union_depth: usize,
    parameter_type_union_depth: usize,
    record_field_type_union_depth: usize,
    generic_bound_depth: usize,
    function_return_union_depth: usize,
    arrow_closure_return_union_depth: usize,
    interface_bound_depth: usize,
    var_initializer_depth: usize,
    return_expr_depth: usize,
    break_expr_depth: usize,
    await_spawn_expr_depth: usize,
    assignment_expr_depth: usize,
    for_iterator_depth: usize,
    while_condition_depth: usize,
    if_condition_depth: usize,
    match_scrutinee_depth: usize,
    match_guard_depth: usize,
    match_arm_body_depth: usize,
    arrow_closure_body_depth: usize,
    test_decl_header_depth: usize,
    type_alias_decl_header_depth: usize,
    interface_decl_header_depth: usize,
    struct_decl_header_depth: usize,
    function_decl_header_depth: usize,
    module_decl_header_depth: usize,
    extern_type_depth: usize,
    extern_var_depth: usize,
    extern_function_depth: usize,
    import_path_depth: usize,
    cast_chain_depth: usize,
    method_chain_depth: usize,
    logical_chain_depth: usize,
    comparison_expr_depth: usize,
    additive_chain_depth: usize,
    multiplicative_chain_depth: usize,
    shift_chain_depth: usize,
    bitwise_and_chain_depth: usize,
    bitwise_xor_chain_depth: usize,
    bitwise_or_chain_depth: usize,
    block_depth: usize,
}

/// For each source line, the bracket depth and block-comment depth at its start.
/// A single forward scan tracks string / line-comment / block-comment state so
/// brackets in those contexts are ignored.
fn scan_line_state(src: &str) -> Vec<LineState> {
    let b = src.as_bytes();
    let mut states = Vec::new();
    let mut depth: usize = 0;
    let mut block: usize = 0; // nested block-comment depth
    let mut i = 0;
    // Record the state at the start of line 0.
    states.push(LineState {
        bracket_depth: depth,
        generic_angle_depth: 0,
        type_alias_union_depth: 0,
        var_type_union_depth: 0,
        parameter_type_union_depth: 0,
        record_field_type_union_depth: 0,
        generic_bound_depth: 0,
        function_return_union_depth: 0,
        arrow_closure_return_union_depth: 0,
        interface_bound_depth: 0,
        var_initializer_depth: 0,
        return_expr_depth: 0,
        break_expr_depth: 0,
        await_spawn_expr_depth: 0,
        assignment_expr_depth: 0,
        for_iterator_depth: 0,
        while_condition_depth: 0,
        if_condition_depth: 0,
        match_scrutinee_depth: 0,
        match_guard_depth: 0,
        match_arm_body_depth: 0,
        arrow_closure_body_depth: 0,
        test_decl_header_depth: 0,
        type_alias_decl_header_depth: 0,
        interface_decl_header_depth: 0,
        struct_decl_header_depth: 0,
        function_decl_header_depth: 0,
        module_decl_header_depth: 0,
        extern_type_depth: 0,
        extern_var_depth: 0,
        extern_function_depth: 0,
        import_path_depth: 0,
        cast_chain_depth: 0,
        method_chain_depth: 0,
        logical_chain_depth: 0,
        comparison_expr_depth: 0,
        additive_chain_depth: 0,
        multiplicative_chain_depth: 0,
        shift_chain_depth: 0,
        bitwise_and_chain_depth: 0,
        bitwise_xor_chain_depth: 0,
        bitwise_or_chain_depth: 0,
        block_depth: block,
    });
    while i < b.len() {
        if block > 0 {
            // Inside a block comment: only `/*` (deeper) and `*/` (shallower)
            // and newlines matter.
            if b[i] == b'\n' {
                i += 1;
                states.push(LineState {
                    bracket_depth: depth,
                    generic_angle_depth: 0,
                    type_alias_union_depth: 0,
                    var_type_union_depth: 0,
                    parameter_type_union_depth: 0,
                    record_field_type_union_depth: 0,
                    generic_bound_depth: 0,
                    function_return_union_depth: 0,
                    arrow_closure_return_union_depth: 0,
                    interface_bound_depth: 0,
                    var_initializer_depth: 0,
                    return_expr_depth: 0,
                    break_expr_depth: 0,
                    await_spawn_expr_depth: 0,
                    assignment_expr_depth: 0,
                    for_iterator_depth: 0,
                    while_condition_depth: 0,
                    if_condition_depth: 0,
                    match_scrutinee_depth: 0,
                    match_guard_depth: 0,
                    match_arm_body_depth: 0,
                    arrow_closure_body_depth: 0,
                    test_decl_header_depth: 0,
                    type_alias_decl_header_depth: 0,
                    interface_decl_header_depth: 0,
                    struct_decl_header_depth: 0,
                    function_decl_header_depth: 0,
                    module_decl_header_depth: 0,
                    extern_type_depth: 0,
                    extern_var_depth: 0,
                    extern_function_depth: 0,
                    import_path_depth: 0,
                    cast_chain_depth: 0,
                    method_chain_depth: 0,
                    logical_chain_depth: 0,
                    comparison_expr_depth: 0,
                    additive_chain_depth: 0,
                    multiplicative_chain_depth: 0,
                    shift_chain_depth: 0,
                    bitwise_and_chain_depth: 0,
                    bitwise_xor_chain_depth: 0,
                    bitwise_or_chain_depth: 0,
                    block_depth: block,
                });
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
                states.push(LineState {
                    bracket_depth: depth,
                    generic_angle_depth: 0,
                    type_alias_union_depth: 0,
                    var_type_union_depth: 0,
                    parameter_type_union_depth: 0,
                    record_field_type_union_depth: 0,
                    generic_bound_depth: 0,
                    function_return_union_depth: 0,
                    arrow_closure_return_union_depth: 0,
                    interface_bound_depth: 0,
                    var_initializer_depth: 0,
                    return_expr_depth: 0,
                    break_expr_depth: 0,
                    await_spawn_expr_depth: 0,
                    assignment_expr_depth: 0,
                    for_iterator_depth: 0,
                    while_condition_depth: 0,
                    if_condition_depth: 0,
                    match_scrutinee_depth: 0,
                    match_guard_depth: 0,
                    match_arm_body_depth: 0,
                    arrow_closure_body_depth: 0,
                    test_decl_header_depth: 0,
                    type_alias_decl_header_depth: 0,
                    interface_decl_header_depth: 0,
                    struct_decl_header_depth: 0,
                    function_decl_header_depth: 0,
                    module_decl_header_depth: 0,
                    extern_type_depth: 0,
                    extern_var_depth: 0,
                    extern_function_depth: 0,
                    import_path_depth: 0,
                    cast_chain_depth: 0,
                    method_chain_depth: 0,
                    logical_chain_depth: 0,
                    comparison_expr_depth: 0,
                    additive_chain_depth: 0,
                    multiplicative_chain_depth: 0,
                    shift_chain_depth: 0,
                    bitwise_and_chain_depth: 0,
                    bitwise_xor_chain_depth: 0,
                    bitwise_or_chain_depth: 0,
                    block_depth: block,
                });
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
    apply_generic_angle_line_depths(src, &mut states);
    apply_type_alias_union_line_depths(src, &mut states);
    apply_var_type_union_line_depths(src, &mut states);
    apply_parameter_type_union_line_depths(src, &mut states);
    apply_record_field_type_union_line_depths(src, &mut states);
    apply_generic_bound_line_depths(src, &mut states);
    apply_function_return_union_line_depths(src, &mut states);
    apply_arrow_closure_return_union_line_depths(src, &mut states);
    apply_interface_bound_line_depths(src, &mut states);
    apply_var_initializer_line_depths(src, &mut states);
    apply_return_expr_line_depths(src, &mut states);
    apply_break_expr_line_depths(src, &mut states);
    apply_await_spawn_expr_line_depths(src, &mut states);
    apply_assignment_expr_line_depths(src, &mut states);
    apply_for_iterator_line_depths(src, &mut states);
    apply_while_condition_line_depths(src, &mut states);
    apply_if_condition_line_depths(src, &mut states);
    apply_match_scrutinee_line_depths(src, &mut states);
    apply_match_guard_line_depths(src, &mut states);
    apply_match_arm_body_line_depths(src, &mut states);
    apply_arrow_closure_body_line_depths(src, &mut states);
    apply_test_decl_header_line_depths(src, &mut states);
    apply_type_alias_decl_header_line_depths(src, &mut states);
    apply_interface_decl_header_line_depths(src, &mut states);
    apply_struct_decl_header_line_depths(src, &mut states);
    apply_function_decl_header_line_depths(src, &mut states);
    apply_module_decl_header_line_depths(src, &mut states);
    apply_extern_type_line_depths(src, &mut states);
    apply_extern_var_line_depths(src, &mut states);
    apply_extern_function_line_depths(src, &mut states);
    apply_import_path_line_depths(src, &mut states);
    apply_named_import_path_line_depths(src, &mut states);
    apply_named_import_closing_path_line_depths(src, &mut states);
    apply_method_chain_line_depths(src, &mut states);
    apply_logical_chain_line_depths(src, &mut states);
    apply_comparison_expr_line_depths(src, &mut states);
    apply_cast_chain_line_depths(src, &mut states);
    apply_additive_chain_line_depths(src, &mut states);
    apply_multiplicative_chain_line_depths(src, &mut states);
    apply_shift_chain_line_depths(src, &mut states);
    apply_bitwise_and_chain_line_depths(src, &mut states);
    apply_bitwise_xor_chain_line_depths(src, &mut states);
    apply_bitwise_or_chain_line_depths(src, &mut states);
    states
}

fn apply_generic_angle_line_depths(src: &str, states: &mut [LineState]) {
    let (tokens, errors) = lex(src, FileId(0));
    if !errors.is_empty() {
        return;
    }
    let tokens = tokens
        .iter()
        .filter(|token| token.kind != TokenKind::Eof)
        .collect::<Vec<_>>();
    if tokens.is_empty() {
        return;
    }
    let angle_roles = classify_angle_roles(&tokens);
    let line_starts = line_starts(src);
    let mut next_line = 1usize;
    let mut angle_depth = 0usize;

    for (idx, token) in tokens.iter().enumerate() {
        let token_start = token.span.range().start;
        while next_line < line_starts.len() && line_starts[next_line] <= token_start {
            if let Some(state) = states.get_mut(next_line) {
                state.generic_angle_depth = angle_depth;
            }
            next_line += 1;
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    while next_line < states.len() {
        states[next_line].generic_angle_depth = angle_depth;
        next_line += 1;
    }
}

fn line_starts(src: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (idx, byte) in src.bytes().enumerate() {
        if byte == b'\n' {
            starts.push(idx + 1);
        }
    }
    starts
}

fn first_non_ws_from_line_start(src: &str, start: usize) -> usize {
    let bytes = src.as_bytes();
    let mut idx = start;
    while idx < bytes.len() && bytes[idx] != b'\n' && bytes[idx].is_ascii_whitespace() {
        idx += 1;
    }
    idx
}

fn apply_type_alias_union_line_depths(src: &str, states: &mut [LineState]) {
    let (tokens, errors) = lex(src, FileId(0));
    if !errors.is_empty() {
        return;
    }
    let tokens = tokens
        .iter()
        .filter(|token| token.kind != TokenKind::Eof)
        .collect::<Vec<_>>();
    if tokens.is_empty() {
        return;
    }
    let angle_roles = classify_angle_roles(&tokens);
    let line_starts = line_starts(src);

    let mut idx = 0usize;
    while idx < tokens.len() {
        if !matches!(tokens[idx].kind, TokenKind::Kw(Keyword::Type)) {
            idx += 1;
            continue;
        }

        let Some(union) = find_type_alias_union_wrap_from(&tokens, &angle_roles, idx) else {
            idx += 1;
            continue;
        };
        let eq_end = tokens[union.eq_idx].span.range().end;
        let semi_start = tokens[union.semi_idx].span.range().start;

        for (line_idx, line_start) in line_starts.iter().enumerate().skip(1) {
            if *line_start > eq_end && *line_start <= semi_start {
                if let Some(state) = states.get_mut(line_idx) {
                    state.type_alias_union_depth = 1;
                }
            }
        }

        idx = union.semi_idx + 1;
    }
}

fn apply_var_type_union_line_depths(src: &str, states: &mut [LineState]) {
    let (tokens, errors) = lex(src, FileId(0));
    if !errors.is_empty() {
        return;
    }
    let tokens = tokens
        .iter()
        .filter(|token| token.kind != TokenKind::Eof)
        .collect::<Vec<_>>();
    if tokens.is_empty() {
        return;
    }
    let angle_roles = classify_angle_roles(&tokens);
    let line_starts = line_starts(src);

    let mut idx = 0usize;
    while idx < tokens.len() {
        let Some(union) = find_var_type_union_wrap_from(&tokens, &angle_roles, idx) else {
            idx += 1;
            continue;
        };
        let colon_end = tokens[union.colon_idx].span.range().end;
        let end_start = tokens[union.end_idx].span.range().start;

        for (line_idx, line_start) in line_starts.iter().enumerate().skip(1) {
            if *line_start > colon_end && *line_start <= end_start {
                if let Some(state) = states.get_mut(line_idx) {
                    state.var_type_union_depth = 1;
                }
            }
        }

        idx = union.end_idx + 1;
    }
}

fn apply_parameter_type_union_line_depths(src: &str, states: &mut [LineState]) {
    let (tokens, errors) = lex(src, FileId(0));
    if !errors.is_empty() {
        return;
    }
    let tokens = tokens
        .iter()
        .filter(|token| token.kind != TokenKind::Eof)
        .collect::<Vec<_>>();
    if tokens.is_empty() {
        return;
    }
    let angle_roles = classify_angle_roles(&tokens);
    let line_starts = line_starts(src);

    let mut idx = 0usize;
    while idx < tokens.len() {
        let Some(list) = find_parameter_list_type_union_wrap_from(&tokens, &angle_roles, idx)
        else {
            idx += 1;
            continue;
        };
        let open_line_end = line_starts
            .iter()
            .copied()
            .find(|line_start| *line_start > tokens[list.open_idx].span.range().start)
            .unwrap_or(src.len() + 1);

        for segment in &list.segments {
            if tokens[segment.start_idx].span.range().start < open_line_end {
                continue;
            }
            let colon_end = tokens[segment.colon_idx].span.range().end;
            let end_start = tokens[segment.end_idx].span.range().start;

            for (line_idx, line_start) in line_starts.iter().enumerate().skip(1) {
                let first_non_ws = first_non_ws_from_line_start(src, *line_start);
                if first_non_ws > colon_end && first_non_ws < end_start {
                    if let Some(state) = states.get_mut(line_idx) {
                        state.parameter_type_union_depth = 1;
                    }
                }
            }
        }

        idx = list.close_idx + 1;
    }
}

fn apply_record_field_type_union_line_depths(src: &str, states: &mut [LineState]) {
    let (tokens, errors) = lex(src, FileId(0));
    if !errors.is_empty() {
        return;
    }
    let tokens = tokens
        .iter()
        .filter(|token| token.kind != TokenKind::Eof)
        .collect::<Vec<_>>();
    if tokens.is_empty() {
        return;
    }
    let angle_roles = classify_angle_roles(&tokens);
    let line_starts = line_starts(src);

    let mut idx = 0usize;
    while idx < tokens.len() {
        let Some(record) = find_record_field_type_union_wrap_from(&tokens, &angle_roles, idx)
        else {
            idx += 1;
            continue;
        };
        let open_line_end = line_starts
            .iter()
            .copied()
            .find(|line_start| *line_start > tokens[record.lbrace_idx].span.range().start)
            .unwrap_or(src.len() + 1);

        for field in &record.fields {
            if field.pipe_indices.is_empty()
                || tokens[field.start_idx].span.range().start < open_line_end
            {
                continue;
            }
            let Some(colon_idx) = field.colon_idx else {
                continue;
            };
            let colon_end = tokens[colon_idx].span.range().end;
            let end_start = tokens[field.end_idx].span.range().start;

            for (line_idx, line_start) in line_starts.iter().enumerate().skip(1) {
                let first_non_ws = first_non_ws_from_line_start(src, *line_start);
                if first_non_ws > colon_end && first_non_ws < end_start {
                    if let Some(state) = states.get_mut(line_idx) {
                        state.record_field_type_union_depth = 1;
                    }
                }
            }
        }

        idx = record.rbrace_idx + 1;
    }
}

fn apply_method_chain_line_depths(src: &str, states: &mut [LineState]) {
    let bytes = src.as_bytes();
    for (line_idx, line_start) in line_starts(src).iter().copied().enumerate() {
        let first_non_ws = first_non_ws_from_line_start(src, line_start);
        if first_non_ws >= bytes.len() || bytes[first_non_ws] != b'.' {
            continue;
        }
        if matches!(bytes.get(first_non_ws + 1), Some(b'.')) {
            continue;
        }
        if let Some(state) = states.get_mut(line_idx) {
            state.method_chain_depth = 1;
        }
    }
}

fn apply_cast_chain_line_depths(src: &str, states: &mut [LineState]) {
    let bytes = src.as_bytes();
    for (line_idx, line_start) in line_starts(src).iter().copied().enumerate() {
        let first_non_ws = first_non_ws_from_line_start(src, line_start);
        if first_non_ws + 2 >= bytes.len() {
            continue;
        }
        let starts_with_cast_keyword =
            bytes[first_non_ws..].starts_with(b"as ") || bytes[first_non_ws..].starts_with(b"is ");
        if starts_with_cast_keyword {
            if let Some(state) = states.get_mut(line_idx) {
                state.cast_chain_depth = 1;
            }
        }
    }
}

fn apply_logical_chain_line_depths(src: &str, states: &mut [LineState]) {
    let bytes = src.as_bytes();
    for (line_idx, line_start) in line_starts(src).iter().copied().enumerate() {
        let first_non_ws = first_non_ws_from_line_start(src, line_start);
        if first_non_ws + 1 >= bytes.len() {
            continue;
        }
        if matches!(
            (bytes[first_non_ws], bytes[first_non_ws + 1]),
            (b'&', b'&') | (b'|', b'|')
        ) {
            if let Some(state) = states.get_mut(line_idx) {
                state.logical_chain_depth = 1;
            }
        }
    }
}

fn apply_comparison_expr_line_depths(src: &str, states: &mut [LineState]) {
    let bytes = src.as_bytes();
    for (line_idx, line_start) in line_starts(src).iter().copied().enumerate() {
        let first_non_ws = first_non_ws_from_line_start(src, line_start);
        if first_non_ws >= bytes.len() {
            continue;
        }
        let state = states.get(line_idx).copied().unwrap_or_default();
        if state.generic_angle_depth > 0 {
            continue;
        }
        let is_two_char_comparison = matches!(
            (bytes[first_non_ws], bytes.get(first_non_ws + 1).copied()),
            (b'=', Some(b'=')) | (b'!', Some(b'=')) | (b'<', Some(b'=')) | (b'>', Some(b'='))
        ) && bytes
            .get(first_non_ws + 2)
            .is_some_and(u8::is_ascii_whitespace);
        let is_single_char_comparison = matches!(bytes[first_non_ws], b'<' | b'>')
            && bytes
                .get(first_non_ws + 1)
                .is_some_and(u8::is_ascii_whitespace);
        if is_two_char_comparison || is_single_char_comparison {
            if let Some(state) = states.get_mut(line_idx) {
                state.comparison_expr_depth = 1;
            }
        }
    }
}

fn apply_additive_chain_line_depths(src: &str, states: &mut [LineState]) {
    let bytes = src.as_bytes();
    for (line_idx, line_start) in line_starts(src).iter().copied().enumerate() {
        let first_non_ws = first_non_ws_from_line_start(src, line_start);
        if first_non_ws >= bytes.len() {
            continue;
        }
        if matches!(bytes[first_non_ws], b'+' | b'-')
            && bytes
                .get(first_non_ws + 1)
                .is_some_and(u8::is_ascii_whitespace)
        {
            if let Some(state) = states.get_mut(line_idx) {
                state.additive_chain_depth = 1;
            }
        }
    }
}

fn apply_multiplicative_chain_line_depths(src: &str, states: &mut [LineState]) {
    let bytes = src.as_bytes();
    for (line_idx, line_start) in line_starts(src).iter().copied().enumerate() {
        let first_non_ws = first_non_ws_from_line_start(src, line_start);
        if first_non_ws >= bytes.len() {
            continue;
        }
        if matches!(bytes[first_non_ws], b'*' | b'/' | b'%')
            && bytes
                .get(first_non_ws + 1)
                .is_some_and(u8::is_ascii_whitespace)
        {
            if let Some(state) = states.get_mut(line_idx) {
                state.multiplicative_chain_depth = 1;
            }
        }
    }
}

fn apply_shift_chain_line_depths(src: &str, states: &mut [LineState]) {
    let bytes = src.as_bytes();
    for (line_idx, line_start) in line_starts(src).iter().copied().enumerate() {
        let first_non_ws = first_non_ws_from_line_start(src, line_start);
        if first_non_ws + 1 >= bytes.len() {
            continue;
        }
        if matches!(
            (bytes[first_non_ws], bytes[first_non_ws + 1]),
            (b'<', b'<') | (b'>', b'>')
        ) && bytes
            .get(first_non_ws + 2)
            .is_some_and(u8::is_ascii_whitespace)
        {
            if let Some(state) = states.get_mut(line_idx) {
                state.shift_chain_depth = 1;
            }
        }
    }
}

fn apply_bitwise_and_chain_line_depths(src: &str, states: &mut [LineState]) {
    let bytes = src.as_bytes();
    for (line_idx, line_start) in line_starts(src).iter().copied().enumerate() {
        let first_non_ws = first_non_ws_from_line_start(src, line_start);
        if first_non_ws >= bytes.len() {
            continue;
        }
        if bytes[first_non_ws] == b'&'
            && !matches!(bytes.get(first_non_ws + 1), Some(b'&'))
            && bytes
                .get(first_non_ws + 1)
                .is_some_and(u8::is_ascii_whitespace)
        {
            if let Some(state) = states.get_mut(line_idx) {
                state.bitwise_and_chain_depth = 1;
            }
        }
    }
}

fn apply_bitwise_xor_chain_line_depths(src: &str, states: &mut [LineState]) {
    let bytes = src.as_bytes();
    for (line_idx, line_start) in line_starts(src).iter().copied().enumerate() {
        let first_non_ws = first_non_ws_from_line_start(src, line_start);
        if first_non_ws >= bytes.len() {
            continue;
        }
        if bytes[first_non_ws] == b'^'
            && bytes
                .get(first_non_ws + 1)
                .is_some_and(u8::is_ascii_whitespace)
        {
            if let Some(state) = states.get_mut(line_idx) {
                state.bitwise_xor_chain_depth = 1;
            }
        }
    }
}

fn apply_bitwise_or_chain_line_depths(src: &str, states: &mut [LineState]) {
    let bytes = src.as_bytes();
    for (line_idx, line_start) in line_starts(src).iter().copied().enumerate() {
        let first_non_ws = first_non_ws_from_line_start(src, line_start);
        if first_non_ws >= bytes.len() {
            continue;
        }
        if bytes[first_non_ws] == b'|'
            && !matches!(bytes.get(first_non_ws + 1), Some(b'|'))
            && bytes
                .get(first_non_ws + 1)
                .is_some_and(u8::is_ascii_whitespace)
        {
            if let Some(state) = states.get_mut(line_idx) {
                state.bitwise_or_chain_depth = 1;
            }
        }
    }
}

fn apply_generic_bound_line_depths(src: &str, states: &mut [LineState]) {
    let (tokens, errors) = lex(src, FileId(0));
    if !errors.is_empty() {
        return;
    }
    let tokens = tokens
        .iter()
        .filter(|token| token.kind != TokenKind::Eof)
        .collect::<Vec<_>>();
    if tokens.is_empty() {
        return;
    }
    let angle_roles = classify_angle_roles(&tokens);
    let line_starts = line_starts(src);

    let mut idx = 0usize;
    while idx < tokens.len() {
        let Some(list) = find_generic_parameter_list_bound_wrap_from(&tokens, &angle_roles, idx)
        else {
            idx += 1;
            continue;
        };
        let open_line_end = line_starts
            .iter()
            .copied()
            .find(|line_start| *line_start > tokens[list.open_idx].span.range().start)
            .unwrap_or(src.len() + 1);

        for segment in &list.segments {
            if segment.plus_indices.is_empty()
                || tokens[segment.start_idx].span.range().start < open_line_end
            {
                continue;
            }
            let Some(colon_idx) = segment.colon_idx else {
                continue;
            };
            let colon_end = tokens[colon_idx].span.range().end;
            let end_start = tokens[segment.end_idx].span.range().start;

            for (line_idx, line_start) in line_starts.iter().enumerate().skip(1) {
                let first_non_ws = first_non_ws_from_line_start(src, *line_start);
                if first_non_ws > colon_end && first_non_ws < end_start {
                    if let Some(state) = states.get_mut(line_idx) {
                        state.generic_bound_depth = 1;
                    }
                }
            }
        }

        idx = list.close_idx + 1;
    }
}

fn apply_function_return_union_line_depths(src: &str, states: &mut [LineState]) {
    let (tokens, errors) = lex(src, FileId(0));
    if !errors.is_empty() {
        return;
    }
    let tokens = tokens
        .iter()
        .filter(|token| token.kind != TokenKind::Eof)
        .collect::<Vec<_>>();
    if tokens.is_empty() {
        return;
    }
    let angle_roles = classify_angle_roles(&tokens);
    let line_starts = line_starts(src);

    for (idx, token) in tokens.iter().enumerate() {
        if !matches!(token.kind, TokenKind::Kw(Keyword::Function)) {
            continue;
        }

        let Some(union) = find_function_return_union_wrap_from(&tokens, &angle_roles, idx) else {
            continue;
        };
        let colon_end = tokens[union.colon_idx].span.range().end;
        let end_start = tokens[union.end_idx].span.range().start;

        for (line_idx, line_start) in line_starts.iter().enumerate().skip(1) {
            if *line_start > colon_end && *line_start <= end_start {
                if let Some(state) = states.get_mut(line_idx) {
                    state.function_return_union_depth = 1;
                }
            }
        }
    }
}

fn apply_arrow_closure_return_union_line_depths(src: &str, states: &mut [LineState]) {
    let (tokens, errors) = lex(src, FileId(0));
    if !errors.is_empty() {
        return;
    }
    let tokens = tokens
        .iter()
        .filter(|token| token.kind != TokenKind::Eof)
        .collect::<Vec<_>>();
    if tokens.is_empty() {
        return;
    }
    let angle_roles = classify_angle_roles(&tokens);
    let line_starts = line_starts(src);

    let mut idx = 0usize;
    while idx < tokens.len() {
        let Some(union) = find_arrow_closure_return_union_wrap_from(&tokens, &angle_roles, idx)
        else {
            idx += 1;
            continue;
        };
        let colon_end = tokens[union.colon_idx].span.range().end;
        let end_start = tokens[union.end_idx].span.range().start;

        for (line_idx, line_start) in line_starts.iter().enumerate().skip(1) {
            if *line_start > colon_end && *line_start <= end_start {
                if let Some(state) = states.get_mut(line_idx) {
                    state.arrow_closure_return_union_depth = 1;
                }
            }
        }

        idx = union.fat_arrow_idx + 1;
    }
}

fn apply_interface_bound_line_depths(src: &str, states: &mut [LineState]) {
    let (tokens, errors) = lex(src, FileId(0));
    if !errors.is_empty() {
        return;
    }
    let tokens = tokens
        .iter()
        .filter(|token| token.kind != TokenKind::Eof)
        .collect::<Vec<_>>();
    if tokens.is_empty() {
        return;
    }
    let angle_roles = classify_angle_roles(&tokens);
    let line_starts = line_starts(src);

    let mut idx = 0usize;
    while idx < tokens.len() {
        if !matches!(
            tokens[idx].kind,
            TokenKind::Kw(Keyword::Interface) | TokenKind::Kw(Keyword::Extend)
        ) {
            idx += 1;
            continue;
        }

        let Some(bounds) = find_interface_bound_wrap_from(&tokens, &angle_roles, idx) else {
            idx += 1;
            continue;
        };
        let colon_end = tokens[bounds.colon_idx].span.range().end;
        let brace_start = tokens[bounds.lbrace_idx].span.range().start;

        for (line_idx, line_start) in line_starts.iter().enumerate().skip(1) {
            let first_non_ws = first_non_ws_from_line_start(src, *line_start);
            if first_non_ws > colon_end && first_non_ws < brace_start {
                if let Some(state) = states.get_mut(line_idx) {
                    state.interface_bound_depth = 1;
                }
            }
        }

        idx = bounds.lbrace_idx + 1;
    }
}

fn apply_var_initializer_line_depths(src: &str, states: &mut [LineState]) {
    let mut previous_was_var_initializer_prefix = false;

    for (line_idx, raw_line) in src.lines().enumerate() {
        if previous_was_var_initializer_prefix {
            if let Some(state) = states.get_mut(line_idx) {
                state.var_initializer_depth = 1;
            }
        }

        let trimmed = raw_line.trim();
        previous_was_var_initializer_prefix = trimmed.starts_with("var ") && trimmed.ends_with('=');
    }
}

fn apply_return_expr_line_depths(src: &str, states: &mut [LineState]) {
    let mut previous_was_return_prefix = false;

    for (line_idx, raw_line) in src.lines().enumerate() {
        if previous_was_return_prefix {
            if let Some(state) = states.get_mut(line_idx) {
                state.return_expr_depth = 1;
            }
        }

        previous_was_return_prefix = raw_line.trim() == "return";
    }
}

fn apply_break_expr_line_depths(src: &str, states: &mut [LineState]) {
    let mut previous_was_break_prefix = false;

    for (line_idx, raw_line) in src.lines().enumerate() {
        if previous_was_break_prefix {
            if let Some(state) = states.get_mut(line_idx) {
                state.break_expr_depth = 1;
            }
        }

        previous_was_break_prefix = raw_line.trim() == "break";
    }
}

fn apply_await_spawn_expr_line_depths(src: &str, states: &mut [LineState]) {
    let mut previous_was_await_or_spawn_prefix = false;

    for (line_idx, raw_line) in src.lines().enumerate() {
        if previous_was_await_or_spawn_prefix {
            if let Some(state) = states.get_mut(line_idx) {
                state.await_spawn_expr_depth = 1;
            }
        }

        let trimmed = raw_line.trim();
        previous_was_await_or_spawn_prefix = trimmed == "await" || trimmed == "spawn";
    }
}

fn apply_assignment_expr_line_depths(src: &str, states: &mut [LineState]) {
    let mut previous_was_assignment_prefix = false;

    for (line_idx, raw_line) in src.lines().enumerate() {
        if previous_was_assignment_prefix {
            if let Some(state) = states.get_mut(line_idx) {
                state.assignment_expr_depth = 1;
            }
        }

        let trimmed = raw_line.trim();
        previous_was_assignment_prefix = trimmed.ends_with('=')
            && !trimmed.starts_with("var ")
            && !trimmed.starts_with("type ")
            && !trimmed.starts_with("function ")
            && !trimmed.starts_with("struct ")
            && !trimmed.starts_with("interface ")
            && !trimmed.starts_with("extend ")
            && !trimmed.starts_with("import ")
            && !trimmed.starts_with("mod ")
            && !trimmed.starts_with("pub ")
            && !trimmed.starts_with("extern ")
            && !trimmed.contains("==")
            && !trimmed.contains("<=")
            && !trimmed.contains(">=")
            && !trimmed.contains("!=");
    }
}

fn apply_for_iterator_line_depths(src: &str, states: &mut [LineState]) {
    let mut previous_was_for_iterator_prefix = false;

    for (line_idx, raw_line) in src.lines().enumerate() {
        if previous_was_for_iterator_prefix {
            if let Some(state) = states.get_mut(line_idx) {
                state.for_iterator_depth = 1;
            }
        }

        let trimmed = raw_line.trim();
        previous_was_for_iterator_prefix = trimmed.starts_with("for ") && trimmed.ends_with(" in");
    }
}

fn apply_while_condition_line_depths(src: &str, states: &mut [LineState]) {
    let mut previous_was_while_prefix = false;

    for (line_idx, raw_line) in src.lines().enumerate() {
        if previous_was_while_prefix {
            if let Some(state) = states.get_mut(line_idx) {
                state.while_condition_depth = 1;
            }
        }

        previous_was_while_prefix = raw_line.trim() == "while";
    }
}

fn apply_if_condition_line_depths(src: &str, states: &mut [LineState]) {
    let mut previous_was_if_prefix = false;

    for (line_idx, raw_line) in src.lines().enumerate() {
        if previous_was_if_prefix {
            if let Some(state) = states.get_mut(line_idx) {
                state.if_condition_depth = 1;
            }
        }

        let trimmed = raw_line.trim();
        previous_was_if_prefix =
            trimmed == "if" || trimmed == "else if" || trimmed.ends_with(" else if");
    }
}

fn apply_match_scrutinee_line_depths(src: &str, states: &mut [LineState]) {
    let mut previous_was_match_prefix = false;

    for (line_idx, raw_line) in src.lines().enumerate() {
        if previous_was_match_prefix {
            if let Some(state) = states.get_mut(line_idx) {
                state.match_scrutinee_depth = 1;
            }
        }

        let trimmed = raw_line.trim();
        previous_was_match_prefix = trimmed == "match" || trimmed.ends_with(" match");
    }
}

fn apply_match_guard_line_depths(src: &str, states: &mut [LineState]) {
    let mut previous_was_match_guard_prefix = false;

    for (line_idx, raw_line) in src.lines().enumerate() {
        if previous_was_match_guard_prefix {
            if let Some(state) = states.get_mut(line_idx) {
                state.match_guard_depth = 1;
            }
        }

        let trimmed = raw_line.trim();
        previous_was_match_guard_prefix = trimmed.ends_with(" if") && !trimmed.ends_with("else if");
    }
}

fn apply_match_arm_body_line_depths(src: &str, states: &mut [LineState]) {
    let mut previous_was_match_arm_prefix = false;

    for (line_idx, raw_line) in src.lines().enumerate() {
        if previous_was_match_arm_prefix {
            if let Some(state) = states.get_mut(line_idx) {
                state.match_arm_body_depth = 1;
            }
        }

        let trimmed = raw_line.trim();
        previous_was_match_arm_prefix = trimmed.ends_with("=>")
            && !arrow_closure_prefix_line_starts_like_closure(trimmed)
            && !trailing_closure_prefix_line_starts_like_closure(trimmed);
    }
}

fn apply_arrow_closure_body_line_depths(src: &str, states: &mut [LineState]) {
    let mut previous_was_arrow_closure_prefix = false;

    for (line_idx, raw_line) in src.lines().enumerate() {
        if previous_was_arrow_closure_prefix {
            if let Some(state) = states.get_mut(line_idx) {
                state.arrow_closure_body_depth = 1;
            }
        }

        let trimmed = raw_line.trim();
        previous_was_arrow_closure_prefix =
            trimmed.ends_with("=>") && arrow_closure_prefix_line_starts_like_closure(trimmed);
    }
}

fn arrow_closure_prefix_line_starts_like_closure(trimmed: &str) -> bool {
    trimmed.starts_with('(')
        || trimmed.contains(" = (")
        || trimmed.contains("= (")
        || trimmed.contains(", (")
}

fn trailing_closure_prefix_line_starts_like_closure(trimmed: &str) -> bool {
    if !trimmed.ends_with("=>") {
        return false;
    }
    let Some((head, header)) = trimmed.rsplit_once('{') else {
        return false;
    };
    let head = head.trim_end();
    if !(head.contains('.') || head.ends_with(')') || head.ends_with(']')) {
        return false;
    }
    let header = header.trim();
    header == "=>"
        || header == "async =>"
        || header.ends_with(" =>")
        || header.ends_with(" async =>")
}

fn apply_test_decl_header_line_depths(src: &str, states: &mut [LineState]) {
    let mut previous_was_test_decl_prefix = false;

    for (line_idx, raw_line) in src.lines().enumerate() {
        if previous_was_test_decl_prefix {
            if let Some(state) = states.get_mut(line_idx) {
                state.test_decl_header_depth = 1;
            }
        }

        let trimmed = raw_line.trim();
        previous_was_test_decl_prefix = trimmed == "test" || trimmed == "bench";
    }
}

fn apply_type_alias_decl_header_line_depths(src: &str, states: &mut [LineState]) {
    let mut previous_was_type_alias_decl_prefix = false;

    for (line_idx, raw_line) in src.lines().enumerate() {
        if previous_was_type_alias_decl_prefix {
            if let Some(state) = states.get_mut(line_idx) {
                state.type_alias_decl_header_depth = 1;
            }
        }

        let trimmed = raw_line.trim();
        previous_was_type_alias_decl_prefix = trimmed == "type" || trimmed == "pub type";
    }
}

fn apply_interface_decl_header_line_depths(src: &str, states: &mut [LineState]) {
    let mut previous_was_interface_decl_prefix = false;

    for (line_idx, raw_line) in src.lines().enumerate() {
        if previous_was_interface_decl_prefix {
            if let Some(state) = states.get_mut(line_idx) {
                state.interface_decl_header_depth = 1;
            }
        }

        let trimmed = raw_line.trim();
        previous_was_interface_decl_prefix =
            trimmed == "interface" || trimmed == "pub interface" || trimmed == "extend";
    }
}

fn apply_struct_decl_header_line_depths(src: &str, states: &mut [LineState]) {
    let mut previous_was_struct_decl_prefix = false;

    for (line_idx, raw_line) in src.lines().enumerate() {
        if previous_was_struct_decl_prefix {
            if let Some(state) = states.get_mut(line_idx) {
                state.struct_decl_header_depth = 1;
            }
        }

        let trimmed = raw_line.trim();
        previous_was_struct_decl_prefix = trimmed == "struct"
            || trimmed == "pub struct"
            || trimmed == "extern struct"
            || trimmed == "pub extern struct";
    }
}

fn apply_function_decl_header_line_depths(src: &str, states: &mut [LineState]) {
    let mut previous_was_function_decl_prefix = false;

    for (line_idx, raw_line) in src.lines().enumerate() {
        if previous_was_function_decl_prefix {
            if let Some(state) = states.get_mut(line_idx) {
                state.function_decl_header_depth = 1;
            }
        }

        let trimmed = raw_line.trim();
        previous_was_function_decl_prefix = trimmed == "function" || trimmed == "pub function";
    }
}

fn apply_module_decl_header_line_depths(src: &str, states: &mut [LineState]) {
    let mut previous_was_module_decl_prefix = false;

    for (line_idx, raw_line) in src.lines().enumerate() {
        if previous_was_module_decl_prefix {
            if let Some(state) = states.get_mut(line_idx) {
                state.module_decl_header_depth = 1;
            }
        }

        let trimmed = raw_line.trim();
        previous_was_module_decl_prefix = trimmed == "mod" || trimmed == "pub mod";
    }
}

fn apply_extern_type_line_depths(src: &str, states: &mut [LineState]) {
    let mut previous_was_extern_type_prefix = false;

    for (line_idx, raw_line) in src.lines().enumerate() {
        if previous_was_extern_type_prefix {
            if let Some(state) = states.get_mut(line_idx) {
                state.extern_type_depth = 1;
            }
        }

        let trimmed = raw_line.trim();
        previous_was_extern_type_prefix = trimmed == "extern type" || trimmed == "pub extern type";
    }
}

fn apply_extern_var_line_depths(src: &str, states: &mut [LineState]) {
    let mut previous_was_extern_var_prefix = false;

    for (line_idx, raw_line) in src.lines().enumerate() {
        if previous_was_extern_var_prefix {
            if let Some(state) = states.get_mut(line_idx) {
                state.extern_var_depth = 1;
            }
        }

        let trimmed = raw_line.trim();
        previous_was_extern_var_prefix = trimmed == "extern var" || trimmed == "pub extern var";
    }
}

fn apply_extern_function_line_depths(src: &str, states: &mut [LineState]) {
    let mut previous_was_extern_function_prefix = false;

    for (line_idx, raw_line) in src.lines().enumerate() {
        if previous_was_extern_function_prefix {
            if let Some(state) = states.get_mut(line_idx) {
                state.extern_function_depth = 1;
            }
        }

        let trimmed = raw_line.trim();
        previous_was_extern_function_prefix =
            trimmed == "extern function" || trimmed == "pub extern function";
    }
}

fn apply_import_path_line_depths(src: &str, states: &mut [LineState]) {
    let mut previous_was_import_prefix = false;

    for (line_idx, raw_line) in src.lines().enumerate() {
        if previous_was_import_prefix {
            if let Some(state) = states.get_mut(line_idx) {
                state.import_path_depth = 1;
            }
        }

        let trimmed = raw_line.trim();
        previous_was_import_prefix = trimmed == "import" || trimmed == "pub import";
    }
}

fn apply_named_import_path_line_depths(src: &str, states: &mut [LineState]) {
    let mut previous_was_named_import_prefix = false;

    for (line_idx, raw_line) in src.lines().enumerate() {
        if previous_was_named_import_prefix {
            if let Some(state) = states.get_mut(line_idx) {
                state.import_path_depth = 1;
            }
        }

        let trimmed = raw_line.trim();
        previous_was_named_import_prefix = (trimmed.starts_with("import {")
            || trimmed.starts_with("pub import {"))
            && trimmed.ends_with('}')
            && !trimmed.contains(" from ");
    }
}

fn apply_named_import_closing_path_line_depths(src: &str, states: &mut [LineState]) {
    let mut in_named_import_list = false;
    let mut previous_was_named_import_close = false;

    for (line_idx, raw_line) in src.lines().enumerate() {
        if previous_was_named_import_close {
            if let Some(state) = states.get_mut(line_idx) {
                state.import_path_depth = 1;
            }
        }

        let trimmed = raw_line.trim();
        previous_was_named_import_close = false;

        if in_named_import_list {
            if trimmed == "}" {
                previous_was_named_import_close = true;
                in_named_import_list = false;
            } else if trimmed.starts_with('}') {
                in_named_import_list = false;
            }
            continue;
        }

        in_named_import_list = (trimmed.starts_with("import {")
            || trimmed.starts_with("pub import {"))
            && !trimmed.ends_with('}')
            && !trimmed.contains(" from ");
    }
}

fn normalize_intra_line_spacing(line: &str, state: LineState) -> String {
    if line.is_empty() || line.trim_start().starts_with("//") {
        return line.to_string();
    }
    if line.contains("${") && (line.contains("/*") || line.contains("//")) {
        return line.to_string();
    }
    if state.import_path_depth > 0 {
        return normalize_import_path_rest_spacing(line);
    }
    if state.type_alias_decl_header_depth > 0
        || state.interface_decl_header_depth > 0
        || state.struct_decl_header_depth > 0
        || state.match_guard_depth > 0
    {
        return line.to_string();
    }

    normalize_generic_bound_fragment_edges(
        &normalize_operator_chain_fragment_spacing(
            &normalize_comment_segmented_spacing(line),
            state,
        ),
        state,
    )
}

fn normalize_operator_chain_fragment_spacing(line: &str, state: LineState) -> String {
    if state.additive_chain_depth == 0
        && state.multiplicative_chain_depth == 0
        && state.shift_chain_depth == 0
        && state.comparison_expr_depth == 0
        && state.bitwise_and_chain_depth == 0
        && state.bitwise_xor_chain_depth == 0
        && state.bitwise_or_chain_depth == 0
    {
        return line.to_string();
    }
    let Some(first) = line.as_bytes().first().copied() else {
        return line.to_string();
    };
    if matches!(first, b'<' | b'>')
        && state.shift_chain_depth > 0
        && line.as_bytes().get(1).copied() == Some(first)
    {
        let rest = line[2..].trim_start();
        if rest.is_empty() {
            return line.to_string();
        }
        return format!("{}{} {}", first as char, first as char, rest);
    }
    if state.comparison_expr_depth > 0 {
        let bytes = line.as_bytes();
        let operator_len = if matches!(
            (bytes[0], bytes.get(1).copied()),
            (b'=', Some(b'=')) | (b'!', Some(b'=')) | (b'<', Some(b'=')) | (b'>', Some(b'='))
        ) {
            2
        } else if matches!(bytes[0], b'<' | b'>') {
            1
        } else {
            0
        };
        if operator_len > 0 {
            let rest = line[operator_len..].trim_start();
            if rest.is_empty() {
                return line.to_string();
            }
            return format!("{} {}", &line[..operator_len], rest);
        }
    }
    if !matches!(first, b'+' | b'-' | b'*' | b'/' | b'%' | b'&' | b'^' | b'|') {
        return line.to_string();
    }
    let rest = line[1..].trim_start();
    if rest.is_empty() {
        return line.to_string();
    }
    format!("{} {}", first as char, rest)
}

fn normalize_generic_bound_fragment_edges(line: &str, state: LineState) -> String {
    let mut out = if looks_like_generic_bound_open_fragment(line) {
        tighten_spaces_around_byte(line, b'<')
    } else {
        line.to_string()
    };
    if state.generic_angle_depth > 0 && out.as_bytes().contains(&b'>') {
        out = tighten_generic_close_fragment_edges(&out);
    }
    out
}

fn looks_like_generic_bound_open_fragment(line: &str) -> bool {
    let bytes = line.as_bytes();
    let Some(lt_idx) = bytes.iter().position(|byte| *byte == b'<') else {
        return false;
    };
    if bytes[lt_idx + 1..].iter().any(|byte| *byte == b'>') {
        return false;
    }
    bytes[lt_idx + 1..].iter().any(|byte| *byte == b':')
}

fn tighten_spaces_around_byte(line: &str, needle: u8) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    let needle = needle as char;
    while let Some(ch) = chars.next() {
        if ch == needle {
            while out.ends_with(' ') {
                out.pop();
            }
            out.push(needle);
            while matches!(chars.peek(), Some(' ')) {
                chars.next();
            }
            continue;
        }
        out.push(ch);
    }
    out
}

fn tighten_generic_close_fragment_edges(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '>' {
            while out.ends_with(' ') {
                out.pop();
            }
            out.push('>');
            let mut spaces = 0usize;
            while matches!(chars.peek(), Some(' ')) {
                spaces += 1;
                chars.next();
            }
            if matches!(chars.peek(), Some('{')) {
                out.push(' ');
            } else if let Some(next) = chars.peek().copied() {
                if !matches!(
                    next,
                    '(' | '[' | '.' | '?' | ',' | ';' | ':' | ')' | ']' | '}' | '>'
                ) {
                    for _ in 0..spaces.max(1) {
                        out.push(' ');
                    }
                }
            }
            continue;
        }
        out.push(ch);
    }
    out
}

fn normalize_block_comment_boundary_tail(line: &str, block_depth: usize) -> String {
    let Some(close_end) = find_block_comment_boundary_close(line, block_depth) else {
        return line.to_string();
    };
    let (comment, code) = line.split_at(close_end);
    let normalized = normalize_comment_segmented_spacing(code.trim());
    if normalized.is_empty() {
        return comment.to_string();
    }
    let mut out = String::with_capacity(comment.len() + normalized.len() + 1);
    out.push_str(comment);
    if !out.ends_with(' ') {
        out.push(' ');
    }
    out.push_str(&normalized);
    out
}

fn find_block_comment_boundary_close(line: &str, block_depth: usize) -> Option<usize> {
    let b = line.as_bytes();
    let mut depth = block_depth;
    let mut i = 0usize;
    while i < b.len() {
        if i + 1 < b.len() && b[i] == b'/' && b[i + 1] == b'*' {
            depth += 1;
            i += 2;
            continue;
        }
        if i + 1 < b.len() && b[i] == b'*' && b[i + 1] == b'/' {
            depth = depth.saturating_sub(1);
            i += 2;
            if depth == 0 {
                return Some(i);
            }
            continue;
        }
        i += 1;
    }
    None
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum LineSegment<'a> {
    Code(&'a str),
    Comment(&'a str),
}

fn normalize_comment_segmented_spacing(line: &str) -> String {
    let Some(segments) = split_comment_segments(line) else {
        return line.to_string();
    };
    join_comment_segments(&segments)
}

fn split_comment_segments(line: &str) -> Option<Vec<LineSegment<'_>>> {
    let b = line.as_bytes();
    let mut segments = Vec::new();
    let mut code_start = 0usize;
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == b'"' {
            i += 1;
            while i < b.len() && b[i] != b'"' {
                if b[i] == b'\\' && i + 1 < b.len() {
                    i += 2;
                } else {
                    i += 1;
                }
            }
            if i < b.len() {
                i += 1;
            }
            continue;
        }
        if b[i] == b'\'' {
            i += 1;
            while i < b.len() && b[i] != b'\'' {
                if b[i] == b'\\' && i + 1 < b.len() {
                    i += 2;
                } else {
                    i += 1;
                }
            }
            if i < b.len() {
                i += 1;
            }
            continue;
        }
        if i + 1 < b.len() && b[i] == b'/' && b[i + 1] == b'/' {
            segments.push(LineSegment::Code(&line[code_start..i]));
            segments.push(LineSegment::Comment(&line[i..]));
            return Some(segments);
        }
        if i + 1 < b.len() && b[i] == b'*' && b[i + 1] == b'/' {
            return None;
        }
        if i + 1 < b.len() && b[i] == b'/' && b[i + 1] == b'*' {
            segments.push(LineSegment::Code(&line[code_start..i]));
            let comment_start = i;
            i += 2;
            let mut depth = 1usize;
            while i < b.len() && depth > 0 {
                if i + 1 < b.len() && b[i] == b'/' && b[i + 1] == b'*' {
                    depth += 1;
                    i += 2;
                } else if i + 1 < b.len() && b[i] == b'*' && b[i + 1] == b'/' {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            if depth > 0 {
                segments.push(LineSegment::Comment(&line[comment_start..]));
                return Some(segments);
            }
            segments.push(LineSegment::Comment(&line[comment_start..i]));
            code_start = i;
            continue;
        }
        i += 1;
    }
    segments.push(LineSegment::Code(&line[code_start..]));
    Some(segments)
}

fn join_comment_segments(segments: &[LineSegment<'_>]) -> String {
    let mut out = String::new();
    for segment in segments {
        match *segment {
            LineSegment::Code(code) => {
                let trimmed = code.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let normalized = normalize_code_spacing(trimmed);
                if normalized.is_empty() {
                    continue;
                }
                if !out.is_empty() && !out.ends_with(' ') {
                    out.push(' ');
                }
                out.push_str(&normalized);
            }
            LineSegment::Comment(comment) => {
                if !out.is_empty() && !out.ends_with(' ') {
                    out.push(' ');
                }
                out.push_str(comment);
            }
        }
    }
    out
}

fn normalize_code_spacing(code: &str) -> String {
    if code.is_empty() {
        return String::new();
    }
    let (tokens, errors) = lex(code, FileId(0));
    if !errors.is_empty() {
        return code.trim().to_string();
    }
    let tokens = tokens
        .iter()
        .filter(|t| t.kind != TokenKind::Eof)
        .collect::<Vec<_>>();
    if tokens.is_empty() {
        return String::new();
    }
    let angle_roles = classify_angle_roles_for_spacing(code, &tokens);

    let mut out = String::new();
    for (i, token) in tokens.iter().enumerate() {
        let curr_text = tok_text(code, token);
        if i > 0 {
            let prev = tokens[i - 1];
            let prev_text = tok_text(code, prev);
            let sep = spacing_between(prev, token, &tokens, &angle_roles, i, prev_text, curr_text);
            out.push_str(sep);
        }
        out.push_str(curr_text);
    }
    out
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DelimitedWrap {
    delimiter: WrapDelimiter,
    open_idx: usize,
    close_idx: usize,
    last_end: usize,
    suffix_start: usize,
    close_on_own_line: bool,
    comma_indices: Vec<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TypeAliasUnionWrap {
    eq_idx: usize,
    semi_idx: usize,
    pipe_indices: Vec<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct VarTypeUnionWrap {
    colon_idx: usize,
    end_idx: usize,
    pipe_indices: Vec<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ParameterTypeUnionWrap {
    colon_idx: usize,
    end_idx: usize,
    pipe_indices: Vec<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ParameterListTypeUnionWrap {
    open_idx: usize,
    close_idx: usize,
    segments: Vec<ParameterSegmentTypeUnionWrap>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ParameterSegmentTypeUnionWrap {
    start_idx: usize,
    colon_idx: usize,
    end_idx: usize,
    pipe_indices: Vec<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RecordFieldTypeUnionWrap {
    lbrace_idx: usize,
    rbrace_idx: usize,
    fields: Vec<RecordFieldTypeUnionSegment>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RecordFieldTypeUnionSegment {
    start_idx: usize,
    colon_idx: Option<usize>,
    end_idx: usize,
    pipe_indices: Vec<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FunctionReturnUnionWrap {
    colon_idx: usize,
    end_idx: usize,
    pipe_indices: Vec<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ArrowClosureReturnUnionWrap {
    colon_idx: usize,
    end_idx: usize,
    fat_arrow_idx: usize,
    pipe_indices: Vec<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GenericBoundWrap {
    colon_idx: usize,
    end_idx: usize,
    plus_indices: Vec<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GenericParameterListBoundWrap {
    open_idx: usize,
    close_idx: usize,
    segments: Vec<GenericParameterSegmentBoundWrap>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GenericParameterSegmentBoundWrap {
    start_idx: usize,
    colon_idx: Option<usize>,
    end_idx: usize,
    plus_indices: Vec<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct InterfaceMemberWrap {
    lbrace_idx: usize,
    rbrace_idx: usize,
    member_ranges: Vec<(usize, usize)>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct InterfaceBoundWrap {
    colon_idx: usize,
    lbrace_idx: usize,
    plus_indices: Vec<usize>,
}

struct VarInitializerWrap {
    eq_idx: usize,
}

struct ReturnExprWrap {
    value_start_idx: usize,
}

struct BreakExprWrap {
    value_start_idx: usize,
}

struct AwaitSpawnExprWrap {
    operand_start_idx: usize,
}

struct AssignmentExprWrap {
    eq_idx: usize,
}

struct ForIteratorWrap {
    in_idx: usize,
    lbrace_idx: usize,
}

struct ForBodyWrap {
    lbrace_idx: usize,
}

struct WhileConditionWrap {
    condition_start_idx: usize,
    lbrace_idx: usize,
}

struct WhileBodyWrap {
    lbrace_idx: usize,
}

struct IfConditionWrap {
    condition_start_idx: usize,
    lbrace_idx: usize,
}

struct IfBodyWrap {
    lbrace_idx: usize,
}

struct MatchScrutineeWrap {
    match_idx: usize,
    lbrace_idx: usize,
}

struct MatchGuardWrap {
    if_idx: usize,
    fat_arrow_idx: usize,
}

struct MatchArmBodyWrap {
    fat_arrow_idx: usize,
}

struct ArrowClosureBodyWrap {
    fat_arrow_idx: usize,
}

struct TrailingClosureBodyWrap {
    fat_arrow_idx: usize,
}

struct ImplicitTrailingClosureBodyWrap {
    lbrace_idx: usize,
}

struct AsyncBlockBodyWrap {
    lbrace_idx: usize,
}

struct BlockExpressionBodyWrap {
    lbrace_idx: usize,
}

struct MacroBlockBodyWrap {
    lbrace_idx: usize,
}

struct LoopBodyWrap {
    lbrace_idx: usize,
}

struct ElseBodyWrap {
    lbrace_idx: usize,
}

struct TestDeclHeaderWrap {
    keyword_idx: usize,
    lbrace_idx: usize,
}

struct TestDeclBodyWrap {
    lbrace_idx: usize,
}

struct TypeAliasDeclHeaderWrap {
    type_idx: usize,
}

struct InterfaceDeclHeaderWrap {
    keyword_idx: usize,
}

struct StructDeclHeaderWrap {
    struct_idx: usize,
}

struct FunctionDeclHeaderWrap {
    function_idx: usize,
}

struct FunctionBodyWrap {
    lbrace_idx: usize,
}

struct AnonFunctionHeaderWrap {
    function_idx: usize,
}

struct ModuleDeclHeaderWrap {
    mod_idx: usize,
}

struct ModuleBodyWrap {
    lbrace_idx: usize,
}

struct ExternTypeWrap {
    type_idx: usize,
    semi_idx: usize,
}

struct ExternVarWrap {
    var_idx: usize,
    semi_idx: usize,
}

struct ExternFunctionWrap {
    function_idx: usize,
    semi_idx: usize,
}

struct ImportPathWrap {
    import_idx: usize,
}

struct NamedImportPathWrap {
    from_idx: usize,
}

struct AttributeArgListWrap {
    lparen_idx: usize,
    rparen_idx: usize,
}

struct CastChainWrap {
    operator_indices: Vec<usize>,
}

struct MethodChainWrap {
    dot_indices: Vec<usize>,
}

struct LogicalChainWrap {
    operator_indices: Vec<usize>,
}

struct ComparisonExprWrap {
    operator_idx: usize,
}

struct AdditiveChainWrap {
    operator_indices: Vec<usize>,
}

struct MultiplicativeChainWrap {
    operator_indices: Vec<usize>,
}

struct ShiftChainWrap {
    operator_indices: Vec<usize>,
}

struct BitwiseAndChainWrap {
    operator_indices: Vec<usize>,
}

struct BitwiseXorChainWrap {
    operator_indices: Vec<usize>,
}

struct BitwiseOrChainWrap {
    operator_indices: Vec<usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WrapDelimiter {
    Paren,
    Bracket,
    Brace(BraceWrapKind),
    Angle,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BraceWrapKind {
    NamedImport,
    MatchArms,
    RecordDecl,
    TypeLed,
    MapLiteralCandidate,
}

fn wrap_long_line(line: &str, base_indent: &str, state: LineState) -> String {
    if base_indent.len() + line.len() <= MAX_LINE_LENGTH {
        return line.to_string();
    }
    if state.type_alias_decl_header_depth > 0
        || state.interface_decl_header_depth > 0
        || state.struct_decl_header_depth > 0
        || state.match_guard_depth > 0
        || state.arrow_closure_body_depth > 0
    {
        return line.to_string();
    }
    if line.contains("//") || line.contains("/*") || line.contains("${") {
        return line.to_string();
    }

    let (tokens, errors) = lex(line, FileId(0));
    if !errors.is_empty() {
        return line.to_string();
    }
    let tokens = tokens
        .iter()
        .filter(|token| token.kind != TokenKind::Eof)
        .collect::<Vec<_>>();
    if tokens.is_empty() {
        return line.to_string();
    }
    let angle_roles = classify_angle_roles(&tokens);
    if let Some(union) = find_type_alias_union_wrap(&tokens, &angle_roles) {
        return wrap_type_alias_union_line(line, base_indent, &tokens, &union);
    }
    if let Some(union) = find_var_type_union_wrap(&tokens, &angle_roles) {
        return wrap_var_type_union_line(line, base_indent, &tokens, &union);
    }
    if let Some(members) = find_interface_member_wrap(&tokens, &angle_roles) {
        return wrap_interface_member_line(line, base_indent, &tokens, &members);
    }
    if let Some(union) = find_function_return_union_wrap(&tokens, &angle_roles) {
        return wrap_function_return_union_line(line, base_indent, &tokens, &union);
    }
    if let Some(union) = find_arrow_closure_return_union_wrap(&tokens, &angle_roles) {
        return wrap_arrow_closure_return_union_line(line, base_indent, &tokens, &union);
    }
    if let Some(list) = find_parameter_list_type_union_wrap(&tokens, &angle_roles) {
        return wrap_parameter_list_type_union_line(line, base_indent, &tokens, &list);
    }
    if let Some(union) = find_parameter_type_union_wrap(&tokens, &angle_roles) {
        return wrap_parameter_type_union_line(line, base_indent, &tokens, &union);
    }
    if let Some(record) = find_record_field_type_union_wrap(&tokens, &angle_roles) {
        return wrap_record_field_type_union_line(line, base_indent, &tokens, &record);
    }
    if let Some(bounds) = find_interface_bound_wrap(&tokens, &angle_roles) {
        return wrap_interface_bound_line(line, base_indent, &tokens, &bounds);
    }
    if let Some(bounds) = find_generic_parameter_list_bound_wrap(&tokens, &angle_roles) {
        return wrap_generic_parameter_list_bound_line(line, base_indent, &tokens, &bounds);
    }
    if let Some(bounds) = find_generic_bound_wrap(&tokens, &angle_roles) {
        return wrap_generic_bound_line(line, base_indent, &tokens, &angle_roles, &bounds);
    }
    if let Some(header) = find_interface_decl_header_wrap(line, &tokens, &angle_roles) {
        return wrap_interface_decl_header_line(line, base_indent, &tokens, &header);
    }
    if let Some(header) = find_struct_decl_header_wrap(&tokens, &angle_roles) {
        return wrap_struct_decl_header_line(line, base_indent, &tokens, &header);
    }
    if let Some(chain) = find_cast_chain_wrap(&tokens, &angle_roles) {
        return wrap_cast_chain_line(line, base_indent, &tokens, &chain);
    }
    if let Some(chain) = find_method_chain_wrap(&tokens, &angle_roles) {
        return wrap_method_chain_line(line, base_indent, &tokens, &chain);
    }
    if let Some(chain) = find_logical_chain_wrap(&tokens, &angle_roles) {
        return wrap_logical_chain_line(line, base_indent, &tokens, &chain);
    }
    if let Some(expr) = find_comparison_expr_wrap(&tokens, &angle_roles) {
        return wrap_comparison_expr_line(line, base_indent, &tokens, &expr);
    }
    if let Some(chain) = find_additive_chain_wrap(&tokens, &angle_roles) {
        return wrap_additive_chain_line(line, base_indent, &tokens, &chain);
    }
    if let Some(chain) = find_multiplicative_chain_wrap(&tokens, &angle_roles) {
        return wrap_multiplicative_chain_line(line, base_indent, &tokens, &chain);
    }
    if let Some(chain) = find_shift_chain_wrap(&tokens, &angle_roles) {
        return wrap_shift_chain_line(line, base_indent, &tokens, &chain);
    }
    if let Some(chain) = find_bitwise_and_chain_wrap(&tokens, &angle_roles) {
        return wrap_bitwise_and_chain_line(line, base_indent, &tokens, &chain);
    }
    if let Some(chain) = find_bitwise_xor_chain_wrap(&tokens, &angle_roles) {
        return wrap_bitwise_xor_chain_line(line, base_indent, &tokens, &chain);
    }
    if let Some(chain) = find_bitwise_or_chain_wrap(&tokens, &angle_roles) {
        return wrap_bitwise_or_chain_line(line, base_indent, &tokens, &chain);
    }
    if let Some(body) = find_test_decl_body_wrap(line, &tokens, &angle_roles) {
        return wrap_test_decl_body_line(line, base_indent, &tokens, &body);
    }
    if let Some(header) = find_test_decl_header_wrap(line, &tokens) {
        return wrap_test_decl_header_line(line, base_indent, &tokens, &header);
    }
    if let Some(body) = find_function_body_wrap(&tokens, &angle_roles) {
        return wrap_function_body_line(line, base_indent, &tokens, &body);
    }
    if let Some(header) = find_function_decl_header_wrap(&tokens, &angle_roles) {
        return wrap_function_decl_header_line(line, base_indent, &tokens, &header);
    }
    if let Some(body) = find_module_body_wrap(&tokens, &angle_roles) {
        return wrap_module_body_line(line, base_indent, &tokens, &body);
    }
    if let Some(header) = find_module_decl_header_wrap(&tokens) {
        return wrap_module_decl_header_line(line, base_indent, &tokens, &header);
    }
    if let Some(extern_type) = find_extern_type_wrap(&tokens) {
        return wrap_extern_type_line(line, base_indent, &tokens, &extern_type);
    }
    if let Some(extern_var) = find_extern_var_wrap(&tokens, &angle_roles) {
        return wrap_extern_var_line(line, base_indent, &tokens, &extern_var);
    }
    if let Some(extern_function) = find_extern_function_wrap(&tokens, &angle_roles) {
        return wrap_extern_function_line(line, base_indent, &tokens, &extern_function);
    }
    if let Some(import) = find_import_path_wrap(&tokens) {
        return wrap_import_path_line(line, base_indent, &tokens, &import);
    }
    if let Some(pair) = find_delimited_wrap(&tokens, &angle_roles) {
        return wrap_delimited_line(line, base_indent, &tokens, &pair);
    }
    if let Some(attribute) = find_attribute_arg_list_wrap(&tokens, &angle_roles) {
        return wrap_attribute_arg_list_line(line, base_indent, &tokens, &attribute);
    }
    if let Some(import) = find_named_import_path_wrap(line, &tokens, &angle_roles) {
        return wrap_named_import_path_line(line, base_indent, &tokens, &import);
    }
    if let Some(header) = find_type_alias_decl_header_wrap(&tokens, &angle_roles) {
        return wrap_type_alias_decl_header_line(line, base_indent, &tokens, &header);
    }
    if let Some(body) = find_macro_block_body_wrap(&tokens, &angle_roles) {
        return wrap_macro_block_body_line(line, base_indent, &tokens, &body);
    }
    if let Some(initializer) = find_var_initializer_wrap(&tokens, &angle_roles) {
        return wrap_var_initializer_line(line, base_indent, &tokens, &initializer);
    }
    if let Some(ret) = find_return_expr_wrap(&tokens, &angle_roles) {
        return wrap_return_expr_line(line, base_indent, &tokens, &ret);
    }
    if let Some(brk) = find_break_expr_wrap(&tokens, &angle_roles) {
        return wrap_break_expr_line(line, base_indent, &tokens, &brk);
    }
    if let Some(expr) = find_await_spawn_expr_wrap(&tokens, &angle_roles) {
        return wrap_await_spawn_expr_line(line, base_indent, &tokens, &expr);
    }
    if let Some(assignment) = find_assignment_expr_wrap(&tokens, &angle_roles) {
        return wrap_assignment_expr_line(line, base_indent, &tokens, &assignment);
    }
    if let Some(body) = find_for_body_wrap(&tokens, &angle_roles) {
        return wrap_for_body_line(line, base_indent, &tokens, &body);
    }
    if let Some(iter) = find_for_iterator_wrap(&tokens, &angle_roles) {
        return wrap_for_iterator_line(line, base_indent, &tokens, &iter);
    }
    if let Some(body) = find_while_body_wrap(&tokens, &angle_roles) {
        return wrap_while_body_line(line, base_indent, &tokens, &body);
    }
    if let Some(cond) = find_while_condition_wrap(&tokens, &angle_roles) {
        return wrap_while_condition_line(line, base_indent, &tokens, &cond);
    }
    if let Some(body) = find_if_body_wrap(&tokens, &angle_roles) {
        return wrap_if_body_line(line, base_indent, &tokens, &body);
    }
    if let Some(cond) = find_if_condition_wrap(&tokens, &angle_roles) {
        return wrap_if_condition_line(line, base_indent, &tokens, &cond);
    }
    if let Some(scrutinee) = find_match_scrutinee_wrap(&tokens, &angle_roles) {
        return wrap_match_scrutinee_line(line, base_indent, &tokens, &scrutinee);
    }
    if let Some(guard) = find_match_guard_wrap(&tokens, &angle_roles) {
        return wrap_match_guard_line(line, base_indent, &tokens, &guard);
    }
    if let Some(body) = find_match_arm_body_wrap(&tokens, &angle_roles) {
        return wrap_match_arm_body_line(line, base_indent, &tokens, &body);
    }
    if let Some(body) = find_arrow_closure_body_wrap(&tokens, &angle_roles) {
        return wrap_arrow_closure_body_line(line, base_indent, &tokens, &body);
    }
    if let Some(body) = find_trailing_closure_body_wrap(&tokens, &angle_roles) {
        return wrap_trailing_closure_body_line(line, base_indent, &tokens, &body);
    }
    if let Some(body) = find_implicit_trailing_closure_body_wrap(&tokens, &angle_roles) {
        return wrap_implicit_trailing_closure_body_line(line, base_indent, &tokens, &body);
    }
    if let Some(body) = find_async_block_body_wrap(&tokens, &angle_roles) {
        return wrap_async_block_body_line(line, base_indent, &tokens, &body);
    }
    if let Some(body) = find_block_expression_body_wrap(&tokens, &angle_roles) {
        return wrap_block_expression_body_line(line, base_indent, &tokens, &body);
    }
    if let Some(body) = find_loop_body_wrap(&tokens, &angle_roles) {
        return wrap_loop_body_line(line, base_indent, &tokens, &body);
    }
    if let Some(body) = find_else_body_wrap(&tokens, &angle_roles) {
        return wrap_else_body_line(line, base_indent, &tokens, &body);
    }
    if let Some(header) = find_anon_function_header_wrap(&tokens, &angle_roles) {
        return wrap_anon_function_header_line(line, base_indent, &tokens, &header);
    }

    line.to_string()
}

fn wrap_delimited_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    pair: &DelimitedWrap,
) -> String {
    let open_end = tokens[pair.open_idx].span.range().end;
    let mut out = String::new();
    out.push_str(&line[..open_end]);

    let continuation_indent = format!("{base_indent}{INDENT}");
    let mut item_start = open_end;
    for comma_idx in &pair.comma_indices {
        let comma_range = tokens[*comma_idx].span.range();
        let item = line[item_start..comma_range.start].trim();
        if item.is_empty() {
            return line.to_string();
        }
        out.push('\n');
        out.push_str(&continuation_indent);
        out.push_str(item);
        out.push(',');
        item_start = comma_range.end;
    }

    let last = line[item_start..pair.last_end].trim();
    if last.is_empty() {
        return line.to_string();
    }
    out.push('\n');
    out.push_str(&continuation_indent);
    out.push_str(last);
    if pair.close_on_own_line {
        out.push('\n');
        out.push_str(base_indent);
    }
    if should_wrap_named_import_closing_path(line, base_indent, tokens, pair) {
        let close_end = tokens[pair.close_idx].span.range().end;
        out.push_str(&line[pair.suffix_start..close_end]);
        let rest = normalize_code_spacing(line[close_end..].trim());
        if rest.is_empty() {
            return line.to_string();
        }
        out.push('\n');
        out.push_str(base_indent);
        out.push_str(INDENT);
        out.push_str(&rest);
        return out;
    }
    out.push_str(&line[pair.suffix_start..]);
    out
}

fn should_wrap_named_import_closing_path(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    pair: &DelimitedWrap,
) -> bool {
    if pair.delimiter != WrapDelimiter::Brace(BraceWrapKind::NamedImport) || !pair.close_on_own_line
    {
        return false;
    }
    let close_end = tokens[pair.close_idx].span.range().end;
    let rest = line[close_end..].trim();
    if !rest.starts_with("from ") {
        return false;
    }
    let closing_line_len =
        base_indent.len() + line[pair.suffix_start..close_end].len() + 1 + rest.len();
    closing_line_len > MAX_LINE_LENGTH
}

fn wrap_type_alias_union_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    union: &TypeAliasUnionWrap,
) -> String {
    let eq_end = tokens[union.eq_idx].span.range().end;
    let semi_start = tokens[union.semi_idx].span.range().start;
    let continuation_indent = format!("{base_indent}{INDENT}");

    let mut out = String::new();
    out.push_str(&line[..eq_end]);

    let mut item_start = eq_end;
    for pipe_idx in &union.pipe_indices {
        let pipe_range = tokens[*pipe_idx].span.range();
        let item = line[item_start..pipe_range.start].trim();
        if item.is_empty() {
            return line.to_string();
        }
        out.push('\n');
        out.push_str(&continuation_indent);
        out.push_str(item);
        out.push_str(" |");
        item_start = pipe_range.end;
    }

    let last = line[item_start..semi_start].trim();
    if last.is_empty() {
        return line.to_string();
    }
    out.push('\n');
    out.push_str(&continuation_indent);
    out.push_str(last);
    out.push_str(&line[semi_start..]);
    out
}

fn wrap_var_type_union_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    union: &VarTypeUnionWrap,
) -> String {
    let colon_end = tokens[union.colon_idx].span.range().end;
    let end_start = tokens[union.end_idx].span.range().start;
    let continuation_indent = format!("{base_indent}{INDENT}");

    let mut out = String::new();
    out.push_str(&line[..colon_end]);

    let mut item_start = colon_end;
    for pipe_idx in &union.pipe_indices {
        let pipe_range = tokens[*pipe_idx].span.range();
        let item = line[item_start..pipe_range.start].trim();
        if item.is_empty() {
            return line.to_string();
        }
        out.push('\n');
        out.push_str(&continuation_indent);
        out.push_str(item);
        out.push_str(" |");
        item_start = pipe_range.end;
    }

    let last = line[item_start..end_start].trim();
    if last.is_empty() {
        return line.to_string();
    }
    out.push('\n');
    out.push_str(&continuation_indent);
    out.push_str(last);
    if !line[end_start..].starts_with(';') && !line[end_start..].starts_with(' ') {
        out.push(' ');
    }
    out.push_str(&line[end_start..]);
    out
}

fn wrap_parameter_type_union_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    union: &ParameterTypeUnionWrap,
) -> String {
    let colon_end = tokens[union.colon_idx].span.range().end;
    let end_start = tokens[union.end_idx].span.range().start;
    let continuation_indent = format!("{base_indent}{INDENT}");

    let mut out = String::new();
    out.push_str(&line[..colon_end]);

    let mut item_start = colon_end;
    for pipe_idx in &union.pipe_indices {
        let pipe_range = tokens[*pipe_idx].span.range();
        let item = line[item_start..pipe_range.start].trim();
        if item.is_empty() {
            return line.to_string();
        }
        out.push('\n');
        out.push_str(&continuation_indent);
        out.push_str(item);
        out.push_str(" |");
        item_start = pipe_range.end;
    }

    let last = line[item_start..end_start].trim();
    if last.is_empty() {
        return line.to_string();
    }
    out.push('\n');
    out.push_str(&continuation_indent);
    out.push_str(last);
    out.push_str(&line[end_start..]);
    out
}

fn wrap_parameter_list_type_union_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    list: &ParameterListTypeUnionWrap,
) -> String {
    let open_end = tokens[list.open_idx].span.range().end;
    let close_start = tokens[list.close_idx].span.range().start;
    let parameter_indent = format!("{base_indent}{INDENT}");
    let union_indent = format!("{parameter_indent}{INDENT}");

    let mut out = String::new();
    out.push_str(&line[..open_end]);

    for segment in &list.segments {
        let segment_start = tokens[segment.start_idx].span.range().start;
        let colon_end = tokens[segment.colon_idx].span.range().end;
        let end_start = tokens[segment.end_idx].span.range().start;
        let has_comma = tokens[segment.end_idx].kind == TokenKind::Comma;

        if segment.pipe_indices.is_empty() {
            let parameter = line[segment_start..end_start].trim();
            if parameter.is_empty() {
                return line.to_string();
            }
            out.push('\n');
            out.push_str(&parameter_indent);
            out.push_str(&normalize_code_spacing(parameter));
            if has_comma {
                out.push(',');
            }
            continue;
        }

        let prefix = line[segment_start..colon_end].trim();
        if prefix.is_empty() {
            return line.to_string();
        }
        out.push('\n');
        out.push_str(&parameter_indent);
        out.push_str(&normalize_code_spacing(prefix));

        let mut item_start = colon_end;
        for pipe_idx in &segment.pipe_indices {
            let pipe_range = tokens[*pipe_idx].span.range();
            let item = line[item_start..pipe_range.start].trim();
            if item.is_empty() {
                return line.to_string();
            }
            out.push('\n');
            out.push_str(&union_indent);
            out.push_str(&normalize_code_spacing(item));
            out.push_str(" |");
            item_start = pipe_range.end;
        }

        let last = line[item_start..end_start].trim();
        if last.is_empty() {
            return line.to_string();
        }
        out.push('\n');
        out.push_str(&union_indent);
        out.push_str(&normalize_code_spacing(last));
        if has_comma {
            out.push(',');
        }
    }

    out.push('\n');
    out.push_str(base_indent);
    out.push_str(&line[close_start..]);
    out
}

fn wrap_record_field_type_union_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    record: &RecordFieldTypeUnionWrap,
) -> String {
    let brace_end = tokens[record.lbrace_idx].span.range().end;
    let close_start = tokens[record.rbrace_idx].span.range().start;
    let field_indent = format!("{base_indent}{INDENT}");
    let union_indent = format!("{field_indent}{INDENT}");

    let mut out = String::new();
    out.push_str(&line[..brace_end]);

    for field in &record.fields {
        let field_start = tokens[field.start_idx].span.range().start;
        let end_start = tokens[field.end_idx].span.range().start;
        let has_comma = tokens[field.end_idx].kind == TokenKind::Comma;

        let Some(colon_idx) = field.colon_idx else {
            let item = line[field_start..end_start].trim();
            if item.is_empty() {
                return line.to_string();
            }
            out.push('\n');
            out.push_str(&field_indent);
            out.push_str(&normalize_code_spacing(item));
            if has_comma {
                out.push(',');
            }
            continue;
        };
        let colon_end = tokens[colon_idx].span.range().end;

        if field.pipe_indices.is_empty() {
            let item = line[field_start..end_start].trim();
            if item.is_empty() {
                return line.to_string();
            }
            out.push('\n');
            out.push_str(&field_indent);
            out.push_str(&normalize_code_spacing(item));
            if has_comma {
                out.push(',');
            }
            continue;
        }

        let prefix = line[field_start..colon_end].trim();
        if prefix.is_empty() {
            return line.to_string();
        }
        out.push('\n');
        out.push_str(&field_indent);
        out.push_str(&normalize_code_spacing(prefix));

        let mut item_start = colon_end;
        for pipe_idx in &field.pipe_indices {
            let pipe_range = tokens[*pipe_idx].span.range();
            let item = line[item_start..pipe_range.start].trim();
            if item.is_empty() {
                return line.to_string();
            }
            out.push('\n');
            out.push_str(&union_indent);
            out.push_str(&normalize_code_spacing(item));
            out.push_str(" |");
            item_start = pipe_range.end;
        }

        let last = line[item_start..end_start].trim();
        if last.is_empty() {
            return line.to_string();
        }
        out.push('\n');
        out.push_str(&union_indent);
        out.push_str(&normalize_code_spacing(last));
        if has_comma {
            out.push(',');
        }
    }

    out.push('\n');
    out.push_str(base_indent);
    out.push_str(&line[close_start..]);
    out
}

fn wrap_generic_bound_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    bounds: &GenericBoundWrap,
) -> String {
    let colon_end = tokens[bounds.colon_idx].span.range().end;
    let end_start = tokens[bounds.end_idx].span.range().start;
    let continuation_indent = format!("{base_indent}{INDENT}");

    let mut out = String::new();
    out.push_str(&render_generic_bound_edge_range(
        line,
        tokens,
        angle_roles,
        0,
        bounds.colon_idx + 1,
    ));

    let mut item_start = colon_end;
    for plus_idx in &bounds.plus_indices {
        let plus_range = tokens[*plus_idx].span.range();
        let item = line[item_start..plus_range.start].trim();
        if item.is_empty() {
            return line.to_string();
        }
        let item = normalize_code_spacing(item);
        out.push('\n');
        out.push_str(&continuation_indent);
        out.push_str(&item);
        out.push_str(" +");
        item_start = plus_range.end;
    }

    let last = line[item_start..end_start].trim();
    if last.is_empty() {
        return line.to_string();
    }
    let last = normalize_code_spacing(last);
    out.push('\n');
    out.push_str(&continuation_indent);
    out.push_str(&last);
    out.push_str(&render_generic_bound_edge_range(
        line,
        tokens,
        angle_roles,
        bounds.end_idx,
        tokens.len(),
    ));
    out
}

fn wrap_generic_parameter_list_bound_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    list: &GenericParameterListBoundWrap,
) -> String {
    let open_end = tokens[list.open_idx].span.range().end;
    let close_start = tokens[list.close_idx].span.range().start;
    let parameter_indent = format!("{base_indent}{INDENT}");
    let bound_indent = format!("{parameter_indent}{INDENT}");

    let mut out = String::new();
    out.push_str(&line[..open_end]);

    for segment in &list.segments {
        let segment_start = tokens[segment.start_idx].span.range().start;
        let end_start = tokens[segment.end_idx].span.range().start;
        let has_comma = tokens[segment.end_idx].kind == TokenKind::Comma;

        let Some(colon_idx) = segment.colon_idx else {
            let parameter = line[segment_start..end_start].trim();
            if parameter.is_empty() {
                return line.to_string();
            }
            out.push('\n');
            out.push_str(&parameter_indent);
            out.push_str(&normalize_code_spacing(parameter));
            if has_comma {
                out.push(',');
            }
            continue;
        };
        let colon_end = tokens[colon_idx].span.range().end;

        if segment.plus_indices.is_empty() {
            let parameter = line[segment_start..end_start].trim();
            if parameter.is_empty() {
                return line.to_string();
            }
            out.push('\n');
            out.push_str(&parameter_indent);
            out.push_str(&normalize_code_spacing(parameter));
            if has_comma {
                out.push(',');
            }
            continue;
        }

        let prefix = line[segment_start..colon_end].trim();
        if prefix.is_empty() {
            return line.to_string();
        }
        out.push('\n');
        out.push_str(&parameter_indent);
        out.push_str(&normalize_code_spacing(prefix));

        let mut item_start = colon_end;
        for plus_idx in &segment.plus_indices {
            let plus_range = tokens[*plus_idx].span.range();
            let item = line[item_start..plus_range.start].trim();
            if item.is_empty() {
                return line.to_string();
            }
            out.push('\n');
            out.push_str(&bound_indent);
            out.push_str(&normalize_code_spacing(item));
            out.push_str(" +");
            item_start = plus_range.end;
        }

        let last = line[item_start..end_start].trim();
        if last.is_empty() {
            return line.to_string();
        }
        out.push('\n');
        out.push_str(&bound_indent);
        out.push_str(&normalize_code_spacing(last));
        if has_comma {
            out.push(',');
        }
    }

    out.push('\n');
    out.push_str(base_indent);
    out.push_str(&line[close_start..]);
    out
}

fn wrap_interface_member_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    members: &InterfaceMemberWrap,
) -> String {
    let brace_end = tokens[members.lbrace_idx].span.range().end;
    let close_start = tokens[members.rbrace_idx].span.range().start;
    let continuation_indent = format!("{base_indent}{INDENT}");

    let mut out = String::new();
    out.push_str(&line[..brace_end]);

    let mut covered_until = brace_end;
    for (start, end) in &members.member_ranges {
        if !line[covered_until..*start].trim().is_empty() {
            return line.to_string();
        }
        let member = line[*start..*end].trim();
        if member.is_empty() {
            return line.to_string();
        }
        out.push('\n');
        out.push_str(&continuation_indent);
        out.push_str(member);
        covered_until = *end;
    }

    if !line[covered_until..close_start].trim().is_empty() {
        return line.to_string();
    }

    out.push('\n');
    out.push_str(base_indent);
    out.push_str(&line[close_start..]);
    out
}

fn wrap_function_return_union_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    union: &FunctionReturnUnionWrap,
) -> String {
    let colon_end = tokens[union.colon_idx].span.range().end;
    let end_start = tokens[union.end_idx].span.range().start;
    let continuation_indent = format!("{base_indent}{INDENT}");

    let mut out = String::new();
    out.push_str(&line[..colon_end]);

    let mut item_start = colon_end;
    for pipe_idx in &union.pipe_indices {
        let pipe_range = tokens[*pipe_idx].span.range();
        let item = line[item_start..pipe_range.start].trim();
        if item.is_empty() {
            return line.to_string();
        }
        out.push('\n');
        out.push_str(&continuation_indent);
        out.push_str(item);
        out.push_str(" |");
        item_start = pipe_range.end;
    }

    let last = line[item_start..end_start].trim();
    if last.is_empty() {
        return line.to_string();
    }
    out.push('\n');
    out.push_str(&continuation_indent);
    out.push_str(last);
    if !line[end_start..].starts_with(';') && !line[end_start..].starts_with(' ') {
        out.push(' ');
    }
    out.push_str(&line[end_start..]);
    out
}

fn wrap_arrow_closure_return_union_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    union: &ArrowClosureReturnUnionWrap,
) -> String {
    let colon_end = tokens[union.colon_idx].span.range().end;
    let end_start = tokens[union.end_idx].span.range().start;
    let continuation_indent = format!("{base_indent}{INDENT}");

    let mut out = String::new();
    out.push_str(&line[..colon_end]);

    let mut item_start = colon_end;
    for pipe_idx in &union.pipe_indices {
        let pipe_range = tokens[*pipe_idx].span.range();
        let item = line[item_start..pipe_range.start].trim();
        if item.is_empty() {
            return line.to_string();
        }
        out.push('\n');
        out.push_str(&continuation_indent);
        out.push_str(item);
        out.push_str(" |");
        item_start = pipe_range.end;
    }

    let last = line[item_start..end_start].trim();
    if last.is_empty() {
        return line.to_string();
    }
    out.push('\n');
    out.push_str(&continuation_indent);
    out.push_str(last);
    if !line[end_start..].starts_with(' ') {
        out.push(' ');
    }
    out.push_str(&line[end_start..]);
    out
}

fn wrap_interface_bound_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    bounds: &InterfaceBoundWrap,
) -> String {
    let colon_end = tokens[bounds.colon_idx].span.range().end;
    let brace_start = tokens[bounds.lbrace_idx].span.range().start;
    let continuation_indent = format!("{base_indent}{INDENT}");

    let mut out = String::new();
    out.push_str(&line[..colon_end]);

    let mut item_start = colon_end;
    for plus_idx in &bounds.plus_indices {
        let plus_range = tokens[*plus_idx].span.range();
        let item = line[item_start..plus_range.start].trim();
        if item.is_empty() {
            return line.to_string();
        }
        let item = normalize_code_spacing(item);
        out.push('\n');
        out.push_str(&continuation_indent);
        out.push_str(&item);
        out.push_str(" +");
        item_start = plus_range.end;
    }

    let last = line[item_start..brace_start].trim();
    if last.is_empty() {
        return line.to_string();
    }
    let last = normalize_code_spacing(last);
    out.push('\n');
    out.push_str(&continuation_indent);
    out.push_str(&last);
    if !line[brace_start..].starts_with(' ') {
        out.push(' ');
    }
    out.push_str(&line[brace_start..]);
    out
}

fn wrap_var_initializer_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    initializer: &VarInitializerWrap,
) -> String {
    let eq_range = tokens[initializer.eq_idx].span.range();
    let prefix = line[..eq_range.end].trim_end();
    let rhs = line[eq_range.end..].trim();
    if prefix.trim().is_empty() || rhs.is_empty() {
        return line.to_string();
    }

    let continuation_indent = format!("{base_indent}{INDENT}");
    format!("{prefix}\n{continuation_indent}{rhs}")
}

fn wrap_return_expr_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    ret: &ReturnExprWrap,
) -> String {
    let value_start = tokens[ret.value_start_idx].span.range().start;
    let rhs = line[value_start..].trim();
    if rhs.is_empty() {
        return line.to_string();
    }

    let continuation_indent = format!("{base_indent}{INDENT}");
    format!("return\n{continuation_indent}{rhs}")
}

fn wrap_break_expr_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    brk: &BreakExprWrap,
) -> String {
    let value_start = tokens[brk.value_start_idx].span.range().start;
    let rhs = line[value_start..].trim();
    if rhs.is_empty() {
        return line.to_string();
    }

    let continuation_indent = format!("{base_indent}{INDENT}");
    format!("break\n{continuation_indent}{rhs}")
}

fn wrap_await_spawn_expr_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    expr: &AwaitSpawnExprWrap,
) -> String {
    let operand_start = tokens[expr.operand_start_idx].span.range().start;
    let prefix = line[..operand_start].trim_end();
    let operand = line[operand_start..].trim();
    if prefix.trim().is_empty() || operand.is_empty() {
        return line.to_string();
    }

    let continuation_indent = format!("{base_indent}{INDENT}");
    format!("{prefix}\n{continuation_indent}{operand}")
}

fn wrap_assignment_expr_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    assignment: &AssignmentExprWrap,
) -> String {
    let eq_range = tokens[assignment.eq_idx].span.range();
    let prefix = line[..eq_range.end].trim_end();
    let rhs = line[eq_range.end..].trim();
    if prefix.trim().is_empty() || rhs.is_empty() {
        return line.to_string();
    }

    let continuation_indent = format!("{base_indent}{INDENT}");
    format!("{prefix}\n{continuation_indent}{rhs}")
}

fn wrap_for_iterator_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    iter: &ForIteratorWrap,
) -> String {
    let in_range = tokens[iter.in_idx].span.range();
    let brace_start = tokens[iter.lbrace_idx].span.range().start;
    let prefix = line[..in_range.end].trim_end();
    let iterator = line[in_range.end..brace_start].trim();
    let rest = line[brace_start..].trim_start();
    if prefix.trim().is_empty() || iterator.is_empty() || rest.is_empty() {
        return line.to_string();
    }

    let continuation_indent = format!("{base_indent}{INDENT}");
    format!("{prefix}\n{continuation_indent}{iterator} {rest}")
}

fn wrap_for_body_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    body: &ForBodyWrap,
) -> String {
    let brace_end = tokens[body.lbrace_idx].span.range().end;
    let prefix = line[..brace_end].trim_end();
    let rest = line[brace_end..].trim();
    if prefix.trim().is_empty() || rest.is_empty() {
        return line.to_string();
    }

    let prefix_depth = unmatched_paren_or_bracket_depth(tokens, body.lbrace_idx);
    let continuation_indent = format!("{base_indent}{}", INDENT.repeat(prefix_depth + 1));
    format!("{prefix}\n{continuation_indent}{rest}")
}

fn wrap_while_condition_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    cond: &WhileConditionWrap,
) -> String {
    let condition_start = tokens[cond.condition_start_idx].span.range().start;
    let brace_start = tokens[cond.lbrace_idx].span.range().start;
    let condition = line[condition_start..brace_start].trim();
    let rest = line[brace_start..].trim_start();
    if condition.is_empty() || rest.is_empty() {
        return line.to_string();
    }

    let continuation_indent = format!("{base_indent}{INDENT}");
    format!("while\n{continuation_indent}{condition} {rest}")
}

fn wrap_if_condition_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    cond: &IfConditionWrap,
) -> String {
    let condition_start = tokens[cond.condition_start_idx].span.range().start;
    let brace_start = tokens[cond.lbrace_idx].span.range().start;
    let prefix = line[..condition_start].trim_end();
    let condition = line[condition_start..brace_start].trim();
    let rest = line[brace_start..].trim_start();
    if prefix.trim().is_empty() || condition.is_empty() || rest.is_empty() {
        return line.to_string();
    }

    let continuation_indent = format!("{base_indent}{INDENT}");
    format!("{prefix}\n{continuation_indent}{condition} {rest}")
}

fn wrap_match_scrutinee_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    scrutinee: &MatchScrutineeWrap,
) -> String {
    let match_end = tokens[scrutinee.match_idx].span.range().end;
    let brace_start = tokens[scrutinee.lbrace_idx].span.range().start;
    let prefix = line[..match_end].trim_end();
    let scrutinee_text = line[match_end..brace_start].trim();
    let rest = line[brace_start..].trim_start();
    if prefix.trim().is_empty() || scrutinee_text.is_empty() || rest.is_empty() {
        return line.to_string();
    }

    let continuation_indent = format!("{base_indent}{INDENT}");
    format!("{prefix}\n{continuation_indent}{scrutinee_text} {rest}")
}

fn wrap_match_guard_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    guard: &MatchGuardWrap,
) -> String {
    let if_end = tokens[guard.if_idx].span.range().end;
    let arrow_start = tokens[guard.fat_arrow_idx].span.range().start;
    let prefix = line[..if_end].trim_end();
    let guard_text = line[if_end..arrow_start].trim();
    let rest = line[arrow_start..].trim_start();
    if prefix.trim().is_empty() || guard_text.is_empty() || rest.is_empty() {
        return line.to_string();
    }

    let continuation_indent = format!("{base_indent}{INDENT}");
    format!("{prefix}\n{continuation_indent}{guard_text} {rest}")
}

fn wrap_match_arm_body_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    body: &MatchArmBodyWrap,
) -> String {
    let arrow_end = tokens[body.fat_arrow_idx].span.range().end;
    let prefix = line[..arrow_end].trim_end();
    let rest = line[arrow_end..].trim();
    if prefix.trim().is_empty() || rest.is_empty() {
        return line.to_string();
    }

    let continuation_indent = format!("{base_indent}{INDENT}");
    format!("{prefix}\n{continuation_indent}{rest}")
}

fn wrap_arrow_closure_body_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    body: &ArrowClosureBodyWrap,
) -> String {
    let arrow_end = tokens[body.fat_arrow_idx].span.range().end;
    let prefix = line[..arrow_end].trim_end();
    let rest = line[arrow_end..].trim();
    if prefix.trim().is_empty() || rest.is_empty() {
        return line.to_string();
    }

    let continuation_indent = format!("{base_indent}{INDENT}");
    format!("{prefix}\n{continuation_indent}{rest}")
}

fn wrap_trailing_closure_body_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    body: &TrailingClosureBodyWrap,
) -> String {
    let arrow_end = tokens[body.fat_arrow_idx].span.range().end;
    let prefix = line[..arrow_end].trim_end();
    let rest = line[arrow_end..].trim();
    if prefix.trim().is_empty() || rest.is_empty() {
        return line.to_string();
    }

    let continuation_indent = format!("{base_indent}{INDENT}");
    format!("{prefix}\n{continuation_indent}{rest}")
}

fn wrap_implicit_trailing_closure_body_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    body: &ImplicitTrailingClosureBodyWrap,
) -> String {
    let brace_end = tokens[body.lbrace_idx].span.range().end;
    let prefix = line[..brace_end].trim_end();
    let rest = line[brace_end..].trim();
    if prefix.trim().is_empty() || rest.is_empty() {
        return line.to_string();
    }

    let continuation_indent = format!("{base_indent}{INDENT}");
    format!("{prefix}\n{continuation_indent}{rest}")
}

fn wrap_async_block_body_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    body: &AsyncBlockBodyWrap,
) -> String {
    let brace_end = tokens[body.lbrace_idx].span.range().end;
    let prefix = line[..brace_end].trim_end();
    let rest = line[brace_end..].trim();
    if prefix.trim().is_empty() || rest.is_empty() {
        return line.to_string();
    }

    let prefix_depth = unmatched_paren_or_bracket_depth(tokens, body.lbrace_idx);
    let continuation_indent = format!("{base_indent}{}", INDENT.repeat(prefix_depth + 1));
    format!("{prefix}\n{continuation_indent}{rest}")
}

fn wrap_block_expression_body_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    body: &BlockExpressionBodyWrap,
) -> String {
    let brace_end = tokens[body.lbrace_idx].span.range().end;
    let prefix = line[..brace_end].trim_end();
    let rest = line[brace_end..].trim();
    if prefix.trim().is_empty() || rest.is_empty() {
        return line.to_string();
    }

    let prefix_depth = unmatched_paren_or_bracket_depth(tokens, body.lbrace_idx);
    let continuation_indent = format!("{base_indent}{}", INDENT.repeat(prefix_depth + 1));
    format!("{prefix}\n{continuation_indent}{rest}")
}

fn wrap_macro_block_body_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    body: &MacroBlockBodyWrap,
) -> String {
    let brace_end = tokens[body.lbrace_idx].span.range().end;
    let prefix = line[..brace_end].trim_end();
    let rest = line[brace_end..].trim();
    if prefix.trim().is_empty() || rest.is_empty() {
        return line.to_string();
    }

    let prefix_depth = unmatched_paren_or_bracket_depth(tokens, body.lbrace_idx);
    let continuation_indent = format!("{base_indent}{}", INDENT.repeat(prefix_depth + 1));
    format!("{prefix}\n{continuation_indent}{rest}")
}

fn wrap_loop_body_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    body: &LoopBodyWrap,
) -> String {
    let brace_end = tokens[body.lbrace_idx].span.range().end;
    let prefix = line[..brace_end].trim_end();
    let rest = line[brace_end..].trim();
    if prefix.trim().is_empty() || rest.is_empty() {
        return line.to_string();
    }

    let prefix_depth = unmatched_paren_or_bracket_depth(tokens, body.lbrace_idx);
    let continuation_indent = format!("{base_indent}{}", INDENT.repeat(prefix_depth + 1));
    format!("{prefix}\n{continuation_indent}{rest}")
}

fn wrap_else_body_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    body: &ElseBodyWrap,
) -> String {
    let brace_end = tokens[body.lbrace_idx].span.range().end;
    let prefix = line[..brace_end].trim_end();
    let rest = line[brace_end..].trim();
    if prefix.trim().is_empty() || rest.is_empty() {
        return line.to_string();
    }

    let prefix_depth = unmatched_paren_or_bracket_depth(tokens, body.lbrace_idx);
    let continuation_indent = format!("{base_indent}{}", INDENT.repeat(prefix_depth + 1));
    format!("{prefix}\n{continuation_indent}{rest}")
}

fn wrap_if_body_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    body: &IfBodyWrap,
) -> String {
    let brace_end = tokens[body.lbrace_idx].span.range().end;
    let prefix = line[..brace_end].trim_end();
    let rest = line[brace_end..].trim();
    if prefix.trim().is_empty() || rest.is_empty() {
        return line.to_string();
    }

    let prefix_depth = unmatched_paren_or_bracket_depth(tokens, body.lbrace_idx);
    let continuation_indent = format!("{base_indent}{}", INDENT.repeat(prefix_depth + 1));
    format!("{prefix}\n{continuation_indent}{rest}")
}

fn wrap_while_body_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    body: &WhileBodyWrap,
) -> String {
    let brace_end = tokens[body.lbrace_idx].span.range().end;
    let prefix = line[..brace_end].trim_end();
    let rest = line[brace_end..].trim();
    if prefix.trim().is_empty() || rest.is_empty() {
        return line.to_string();
    }

    let prefix_depth = unmatched_paren_or_bracket_depth(tokens, body.lbrace_idx);
    let continuation_indent = format!("{base_indent}{}", INDENT.repeat(prefix_depth + 1));
    format!("{prefix}\n{continuation_indent}{rest}")
}

fn unmatched_paren_or_bracket_depth(tokens: &[&Token], end_idx: usize) -> usize {
    let mut depth = 0usize;
    for token in tokens.iter().take(end_idx) {
        match token.kind {
            TokenKind::LParen | TokenKind::LBracket => depth += 1,
            TokenKind::RParen | TokenKind::RBracket => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    depth
}

fn wrap_test_decl_header_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    header: &TestDeclHeaderWrap,
) -> String {
    let keyword_end = tokens[header.keyword_idx].span.range().end;
    let brace_start = tokens[header.lbrace_idx].span.range().start;
    let prefix = line[..keyword_end].trim_end();
    let name = line[keyword_end..brace_start].trim();
    let rest = line[brace_start..].trim_start();
    if prefix.trim().is_empty() || name.is_empty() || rest.is_empty() {
        return line.to_string();
    }

    let continuation_indent = format!("{base_indent}{INDENT}");
    format!("{prefix}\n{continuation_indent}{name} {rest}")
}

fn wrap_test_decl_body_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    body: &TestDeclBodyWrap,
) -> String {
    let brace_end = tokens[body.lbrace_idx].span.range().end;
    let prefix = line[..brace_end].trim_end();
    let rest = line[brace_end..].trim();
    if prefix.trim().is_empty() || rest.is_empty() {
        return line.to_string();
    }

    let prefix_depth = unmatched_paren_or_bracket_depth(tokens, body.lbrace_idx);
    let continuation_indent = format!("{base_indent}{}", INDENT.repeat(prefix_depth + 1));
    format!("{prefix}\n{continuation_indent}{rest}")
}

fn wrap_type_alias_decl_header_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    header: &TypeAliasDeclHeaderWrap,
) -> String {
    let type_end = tokens[header.type_idx].span.range().end;
    let prefix = line[..type_end].trim_end();
    let rest = tighten_generic_close_fragment_edges(&tighten_spaces_around_byte(
        line[type_end..].trim(),
        b'<',
    ));
    if prefix.trim().is_empty() || rest.is_empty() {
        return line.to_string();
    }

    let continuation_indent = format!("{base_indent}{INDENT}");
    format!("{prefix}\n{continuation_indent}{rest}")
}

fn wrap_interface_decl_header_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    header: &InterfaceDeclHeaderWrap,
) -> String {
    let keyword_end = tokens[header.keyword_idx].span.range().end;
    let prefix = line[..keyword_end].trim_end();
    let rest = tighten_interface_decl_header_rest(line[keyword_end..].trim());
    if prefix.trim().is_empty() || rest.is_empty() {
        return line.to_string();
    }

    let continuation_indent = format!("{base_indent}{INDENT}");
    format!("{prefix}\n{continuation_indent}{rest}")
}

fn tighten_interface_decl_header_rest(rest: &str) -> String {
    let Some(brace_idx) = rest.find('{') else {
        return tighten_generic_close_fragment_edges(&tighten_spaces_around_byte(rest, b'<'));
    };
    let header = rest[..brace_idx].trim_end();
    let body = rest[brace_idx..].trim_start();
    let header = tighten_generic_close_fragment_edges(&tighten_spaces_around_byte(header, b'<'));
    if header.is_empty() {
        body.to_string()
    } else {
        format!("{header} {body}")
    }
}

fn wrap_struct_decl_header_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    header: &StructDeclHeaderWrap,
) -> String {
    let struct_end = tokens[header.struct_idx].span.range().end;
    let prefix = line[..struct_end].trim_end();
    let rest = tighten_interface_decl_header_rest(line[struct_end..].trim());
    if prefix.trim().is_empty() || rest.is_empty() {
        return line.to_string();
    }

    let continuation_indent = format!("{base_indent}{INDENT}");
    format!("{prefix}\n{continuation_indent}{rest}")
}

fn wrap_function_decl_header_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    header: &FunctionDeclHeaderWrap,
) -> String {
    let function_end = tokens[header.function_idx].span.range().end;
    let prefix = line[..function_end].trim_end();
    let rest = line[function_end..].trim();
    if prefix.trim().is_empty() || rest.is_empty() {
        return line.to_string();
    }

    let continuation_indent = format!("{base_indent}{INDENT}");
    format!("{prefix}\n{continuation_indent}{rest}")
}

fn wrap_function_body_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    body: &FunctionBodyWrap,
) -> String {
    let brace_end = tokens[body.lbrace_idx].span.range().end;
    let prefix = line[..brace_end].trim_end();
    let rest = line[brace_end..].trim();
    if prefix.trim().is_empty() || rest.is_empty() {
        return line.to_string();
    }

    let prefix_depth = unmatched_paren_or_bracket_depth(tokens, body.lbrace_idx);
    let continuation_indent = format!("{base_indent}{}", INDENT.repeat(prefix_depth + 1));
    format!("{prefix}\n{continuation_indent}{rest}")
}

fn wrap_anon_function_header_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    header: &AnonFunctionHeaderWrap,
) -> String {
    let function_end = tokens[header.function_idx].span.range().end;
    let prefix = line[..function_end].trim_end();
    let rest = line[function_end..].trim();
    if prefix.trim().is_empty() || rest.is_empty() {
        return line.to_string();
    }

    let continuation_indent = format!("{base_indent}{INDENT}");
    format!("{prefix}\n{continuation_indent}{rest}")
}

fn wrap_module_decl_header_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    header: &ModuleDeclHeaderWrap,
) -> String {
    let mod_end = tokens[header.mod_idx].span.range().end;
    let prefix = line[..mod_end].trim_end();
    let rest = line[mod_end..].trim();
    if prefix.trim().is_empty() || rest.is_empty() {
        return line.to_string();
    }

    let continuation_indent = format!("{base_indent}{INDENT}");
    format!("{prefix}\n{continuation_indent}{rest}")
}

fn wrap_module_body_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    body: &ModuleBodyWrap,
) -> String {
    let brace_end = tokens[body.lbrace_idx].span.range().end;
    let prefix = line[..brace_end].trim_end();
    let rest = line[brace_end..].trim();
    if prefix.trim().is_empty() || rest.is_empty() {
        return line.to_string();
    }

    let prefix_depth = unmatched_paren_or_bracket_depth(tokens, body.lbrace_idx);
    let continuation_indent = format!("{base_indent}{}", INDENT.repeat(prefix_depth + 1));
    format!("{prefix}\n{continuation_indent}{rest}")
}

fn wrap_extern_type_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    extern_type: &ExternTypeWrap,
) -> String {
    let type_end = tokens[extern_type.type_idx].span.range().end;
    let semi_end = tokens[extern_type.semi_idx].span.range().end;
    let prefix = line[..type_end].trim_end();
    let name = line[type_end..semi_end].trim();
    if prefix.trim().is_empty() || name.is_empty() {
        return line.to_string();
    }

    let continuation_indent = format!("{base_indent}{INDENT}");
    format!("{prefix}\n{continuation_indent}{name}")
}

fn wrap_extern_var_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    extern_var: &ExternVarWrap,
) -> String {
    let var_end = tokens[extern_var.var_idx].span.range().end;
    let semi_end = tokens[extern_var.semi_idx].span.range().end;
    let prefix = line[..var_end].trim_end();
    let rest = line[var_end..semi_end].trim();
    if prefix.trim().is_empty() || rest.is_empty() {
        return line.to_string();
    }

    let continuation_indent = format!("{base_indent}{INDENT}");
    format!("{prefix}\n{continuation_indent}{rest}")
}

fn wrap_extern_function_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    extern_function: &ExternFunctionWrap,
) -> String {
    let function_end = tokens[extern_function.function_idx].span.range().end;
    let semi_end = tokens[extern_function.semi_idx].span.range().end;
    let prefix = line[..function_end].trim_end();
    let rest = line[function_end..semi_end].trim();
    if prefix.trim().is_empty() || rest.is_empty() {
        return line.to_string();
    }

    let continuation_indent = format!("{base_indent}{INDENT}");
    format!("{prefix}\n{continuation_indent}{rest}")
}

fn wrap_import_path_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    import: &ImportPathWrap,
) -> String {
    let import_end = tokens[import.import_idx].span.range().end;
    let prefix = line[..import_end].trim_end();
    let rest = normalize_import_path_rest_spacing(line[import_end..].trim());
    if prefix.trim().is_empty() || rest.is_empty() {
        return line.to_string();
    }

    let continuation_indent = format!("{base_indent}{INDENT}");
    format!("{prefix}\n{continuation_indent}{rest}")
}

fn wrap_named_import_path_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    import: &NamedImportPathWrap,
) -> String {
    let from_start = tokens[import.from_idx].span.range().start;
    let prefix = line[..from_start].trim_end();
    let rest = normalize_code_spacing(line[from_start..].trim());
    if prefix.trim().is_empty() || rest.is_empty() {
        return line.to_string();
    }

    let continuation_indent = format!("{base_indent}{INDENT}");
    format!("{prefix}\n{continuation_indent}{rest}")
}

fn wrap_attribute_arg_list_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    attribute: &AttributeArgListWrap,
) -> String {
    let open_end = tokens[attribute.lparen_idx].span.range().end;
    let close_start = tokens[attribute.rparen_idx].span.range().start;
    let prefix = line[..open_end].trim_end();
    let arg = normalize_code_spacing(line[open_end..close_start].trim());
    if prefix.trim().is_empty() || arg.is_empty() {
        return line.to_string();
    }

    let continuation_indent = format!("{base_indent}{INDENT}");
    format!(
        "{prefix}\n{continuation_indent}{arg}\n{base_indent}{}",
        &line[close_start..]
    )
}

fn normalize_import_path_rest_spacing(rest: &str) -> String {
    let rest = rest.trim();
    let Some(path_end) = leading_string_literal_end(rest) else {
        return normalize_code_spacing(rest);
    };
    let path = &rest[..path_end];
    let suffix = rest[path_end..].trim_start();
    if suffix.is_empty() {
        return path.to_string();
    }
    if suffix.starts_with(';') {
        return format!("{path}{suffix}");
    }
    if starts_with_keyword_text(suffix, "as") {
        return format!("{path} {suffix}");
    }

    let suffix = normalize_code_spacing(suffix);
    if suffix.is_empty() {
        path.to_string()
    } else if suffix.starts_with(';') {
        format!("{path}{suffix}")
    } else {
        format!("{path} {suffix}")
    }
}

fn leading_string_literal_end(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    if bytes.first().copied() != Some(b'"') {
        return None;
    }

    let mut i = 1usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if i + 1 < bytes.len() => i += 2,
            b'"' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

fn starts_with_keyword_text(text: &str, keyword: &str) -> bool {
    let Some(rest) = text.strip_prefix(keyword) else {
        return false;
    };
    rest.as_bytes()
        .first()
        .is_none_or(|byte| !byte.is_ascii_alphanumeric() && *byte != b'_')
}

fn wrap_cast_chain_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    chain: &CastChainWrap,
) -> String {
    wrap_infix_chain_line(line, base_indent, tokens, &chain.operator_indices)
}

fn wrap_method_chain_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    chain: &MethodChainWrap,
) -> String {
    let Some(first_dot_idx) = chain.dot_indices.first().copied() else {
        return line.to_string();
    };
    let first_dot_start = tokens[first_dot_idx].span.range().start;
    let prefix = line[..first_dot_start].trim_end();
    if prefix.trim().is_empty() {
        return line.to_string();
    }

    let continuation_indent = format!("{base_indent}{INDENT}");
    let mut out = String::new();
    out.push_str(prefix);

    for (idx, dot_idx) in chain.dot_indices.iter().copied().enumerate() {
        let segment_start = tokens[dot_idx].span.range().start;
        let segment_end = chain
            .dot_indices
            .get(idx + 1)
            .map(|next_dot_idx| tokens[*next_dot_idx].span.range().start)
            .unwrap_or(line.len());
        let segment = line[segment_start..segment_end].trim();
        if segment.is_empty() {
            return line.to_string();
        }
        out.push('\n');
        out.push_str(&continuation_indent);
        out.push_str(segment);
    }

    out
}

fn wrap_logical_chain_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    chain: &LogicalChainWrap,
) -> String {
    wrap_infix_chain_line(line, base_indent, tokens, &chain.operator_indices)
}

fn wrap_comparison_expr_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    expr: &ComparisonExprWrap,
) -> String {
    wrap_infix_chain_line(line, base_indent, tokens, &[expr.operator_idx])
}

fn wrap_additive_chain_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    chain: &AdditiveChainWrap,
) -> String {
    wrap_infix_chain_line(line, base_indent, tokens, &chain.operator_indices)
}

fn wrap_multiplicative_chain_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    chain: &MultiplicativeChainWrap,
) -> String {
    wrap_infix_chain_line(line, base_indent, tokens, &chain.operator_indices)
}

fn wrap_shift_chain_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    chain: &ShiftChainWrap,
) -> String {
    wrap_infix_chain_line(line, base_indent, tokens, &chain.operator_indices)
}

fn wrap_bitwise_and_chain_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    chain: &BitwiseAndChainWrap,
) -> String {
    wrap_infix_chain_line(line, base_indent, tokens, &chain.operator_indices)
}

fn wrap_bitwise_xor_chain_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    chain: &BitwiseXorChainWrap,
) -> String {
    wrap_infix_chain_line(line, base_indent, tokens, &chain.operator_indices)
}

fn wrap_bitwise_or_chain_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    chain: &BitwiseOrChainWrap,
) -> String {
    wrap_infix_chain_line(line, base_indent, tokens, &chain.operator_indices)
}

fn wrap_infix_chain_line(
    line: &str,
    base_indent: &str,
    tokens: &[&Token],
    operator_indices: &[usize],
) -> String {
    let Some(first_operator_idx) = operator_indices.first().copied() else {
        return line.to_string();
    };
    let first_operator_start = tokens[first_operator_idx].span.range().start;
    let prefix = line[..first_operator_start].trim_end();
    if prefix.trim().is_empty() {
        return line.to_string();
    }

    let continuation_indent = format!("{base_indent}{INDENT}");
    let mut out = String::new();
    out.push_str(prefix);

    for (idx, operator_idx) in operator_indices.iter().copied().enumerate() {
        let operator_range = tokens[operator_idx].span.range();
        let segment_end = operator_indices
            .get(idx + 1)
            .map(|next_operator_idx| tokens[*next_operator_idx].span.range().start)
            .unwrap_or(line.len());
        let rhs = line[operator_range.end..segment_end].trim();
        if rhs.is_empty() {
            return line.to_string();
        }
        out.push('\n');
        out.push_str(&continuation_indent);
        out.push_str(&line[operator_range.clone()]);
        out.push(' ');
        out.push_str(rhs);
    }

    out
}

fn find_type_alias_union_wrap(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
) -> Option<TypeAliasUnionWrap> {
    find_type_alias_union_wrap_from(tokens, angle_roles, 0)
}

fn find_type_alias_union_wrap_from(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    type_idx: usize,
) -> Option<TypeAliasUnionWrap> {
    if !matches!(
        tokens.get(type_idx).map(|token| token.kind),
        Some(TokenKind::Kw(Keyword::Type))
    ) {
        return None;
    }

    let eq_idx = tokens
        .iter()
        .enumerate()
        .skip(type_idx + 1)
        .find_map(|(idx, token)| (token.kind == TokenKind::Eq).then_some(idx))?;
    let semi_idx = tokens
        .iter()
        .enumerate()
        .skip(eq_idx + 1)
        .find_map(|(idx, token)| (token.kind == TokenKind::Semi).then_some(idx))?;
    let pipe_indices = top_level_pipes_in_range(tokens, angle_roles, eq_idx, semi_idx);
    if pipe_indices.is_empty() {
        return None;
    }
    Some(TypeAliasUnionWrap {
        eq_idx,
        semi_idx,
        pipe_indices,
    })
}

fn find_function_return_union_wrap(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
) -> Option<FunctionReturnUnionWrap> {
    tokens.iter().enumerate().find_map(|(idx, token)| {
        matches!(token.kind, TokenKind::Kw(Keyword::Function))
            .then(|| find_function_return_union_wrap_from(tokens, angle_roles, idx))
            .flatten()
    })
}

fn find_var_type_union_wrap(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
) -> Option<VarTypeUnionWrap> {
    tokens.iter().enumerate().find_map(|(idx, token)| {
        matches!(token.kind, TokenKind::Kw(Keyword::Var))
            .then(|| find_var_type_union_wrap_from(tokens, angle_roles, idx))
            .flatten()
    })
}

fn find_var_type_union_wrap_from(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    var_idx: usize,
) -> Option<VarTypeUnionWrap> {
    if !matches!(
        tokens.get(var_idx).map(|token| token.kind),
        Some(TokenKind::Kw(Keyword::Var))
    ) {
        return None;
    }

    let mut colon_idx = None;
    let mut end_idx = None;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in var_idx + 1..tokens.len() {
        if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 && angle_depth == 0 {
            match tokens[idx].kind {
                TokenKind::Colon if colon_idx.is_none() => {
                    colon_idx = Some(idx);
                }
                TokenKind::Eq | TokenKind::Semi if colon_idx.is_some() => {
                    end_idx = Some(idx);
                    break;
                }
                TokenKind::FatArrow if colon_idx.is_some() => return None,
                TokenKind::Kw(Keyword::Var) | TokenKind::Kw(Keyword::Function) => return None,
                _ => {}
            }
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    let colon_idx = colon_idx?;
    let end_idx = end_idx?;
    if colon_idx >= end_idx {
        return None;
    }
    let pipe_indices = top_level_pipes_in_range(tokens, angle_roles, colon_idx, end_idx);
    if pipe_indices.is_empty() {
        return None;
    }
    Some(VarTypeUnionWrap {
        colon_idx,
        end_idx,
        pipe_indices,
    })
}

fn find_parameter_type_union_wrap(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
) -> Option<ParameterTypeUnionWrap> {
    tokens.iter().enumerate().find_map(|(idx, token)| {
        (token.kind == TokenKind::LParen)
            .then(|| find_parameter_type_union_wrap_from(tokens, angle_roles, idx))
            .flatten()
    })
}

fn find_parameter_list_type_union_wrap(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
) -> Option<ParameterListTypeUnionWrap> {
    tokens.iter().enumerate().find_map(|(idx, token)| {
        (token.kind == TokenKind::LParen)
            .then(|| find_parameter_list_type_union_wrap_from(tokens, angle_roles, idx))
            .flatten()
    })
}

fn find_parameter_list_type_union_wrap_from(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    open_idx: usize,
) -> Option<ParameterListTypeUnionWrap> {
    if tokens.get(open_idx).map(|token| token.kind) != Some(TokenKind::LParen) {
        return None;
    }
    let close_idx = matching_paren_close(tokens, open_idx)?;
    if !plausible_parameter_list_follow(tokens, close_idx) {
        return None;
    }
    let segments =
        top_level_parameter_type_union_segments(tokens, angle_roles, open_idx, close_idx);
    if segments.len() < 2
        || segments
            .iter()
            .all(|segment| segment.pipe_indices.is_empty())
    {
        return None;
    }
    Some(ParameterListTypeUnionWrap {
        open_idx,
        close_idx,
        segments,
    })
}

fn find_parameter_type_union_wrap_from(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    open_idx: usize,
) -> Option<ParameterTypeUnionWrap> {
    if tokens.get(open_idx).map(|token| token.kind) != Some(TokenKind::LParen) {
        return None;
    }
    let close_idx = matching_paren_close(tokens, open_idx)?;
    if !plausible_parameter_list_follow(tokens, close_idx) {
        return None;
    }

    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in open_idx + 1..close_idx {
        let top_level =
            paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 && angle_depth == 0;
        if top_level && tokens[idx].kind == TokenKind::Colon {
            let end_idx = top_level_param_segment_end(tokens, angle_roles, idx, close_idx);
            let pipe_indices = top_level_pipes_in_range(tokens, angle_roles, idx, end_idx);
            if !pipe_indices.is_empty() {
                return Some(ParameterTypeUnionWrap {
                    colon_idx: idx,
                    end_idx,
                    pipe_indices,
                });
            }
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    None
}

fn top_level_parameter_type_union_segments(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    open_idx: usize,
    close_idx: usize,
) -> Vec<ParameterSegmentTypeUnionWrap> {
    let mut segments = Vec::new();
    let mut segment_start = open_idx + 1;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in open_idx + 1..=close_idx {
        let is_boundary = idx == close_idx
            || (paren_depth == 0
                && bracket_depth == 0
                && brace_depth == 0
                && angle_depth == 0
                && tokens[idx].kind == TokenKind::Comma);
        if is_boundary {
            if let Some(segment) =
                parameter_type_union_segment(tokens, angle_roles, segment_start, idx)
            {
                segments.push(segment);
            }
            segment_start = idx + 1;
            continue;
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    segments
}

fn parameter_type_union_segment(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    start_idx: usize,
    end_idx: usize,
) -> Option<ParameterSegmentTypeUnionWrap> {
    if start_idx >= end_idx {
        return None;
    }
    let colon_idx = top_level_colon_in_range(tokens, angle_roles, start_idx, end_idx)?;
    let pipe_indices = top_level_pipes_in_range(tokens, angle_roles, colon_idx, end_idx);
    Some(ParameterSegmentTypeUnionWrap {
        start_idx,
        colon_idx,
        end_idx,
        pipe_indices,
    })
}

fn find_record_field_type_union_wrap(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
) -> Option<RecordFieldTypeUnionWrap> {
    tokens.iter().enumerate().find_map(|(idx, token)| {
        (token.kind == TokenKind::LBrace)
            .then(|| find_record_field_type_union_wrap_from(tokens, angle_roles, idx))
            .flatten()
    })
}

fn find_record_field_type_union_wrap_from(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    lbrace_idx: usize,
) -> Option<RecordFieldTypeUnionWrap> {
    if tokens.get(lbrace_idx).map(|token| token.kind) != Some(TokenKind::LBrace)
        || !plausible_record_decl_brace_wrap_open(tokens, angle_roles, lbrace_idx)
    {
        return None;
    }
    let rbrace_idx = matching_brace_close(tokens, lbrace_idx)?;
    let fields =
        top_level_record_field_type_union_segments(tokens, angle_roles, lbrace_idx, rbrace_idx);
    if fields.is_empty() || fields.iter().all(|field| field.pipe_indices.is_empty()) {
        return None;
    }
    Some(RecordFieldTypeUnionWrap {
        lbrace_idx,
        rbrace_idx,
        fields,
    })
}

fn top_level_record_field_type_union_segments(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    lbrace_idx: usize,
    rbrace_idx: usize,
) -> Vec<RecordFieldTypeUnionSegment> {
    let mut fields = Vec::new();
    let mut field_start = lbrace_idx + 1;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in lbrace_idx + 1..=rbrace_idx {
        let is_boundary = idx == rbrace_idx
            || (paren_depth == 0
                && bracket_depth == 0
                && brace_depth == 0
                && angle_depth == 0
                && tokens[idx].kind == TokenKind::Comma);
        if is_boundary {
            if let Some(field) =
                record_field_type_union_segment(tokens, angle_roles, field_start, idx)
            {
                fields.push(field);
            }
            field_start = idx + 1;
            continue;
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    fields
}

fn record_field_type_union_segment(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    start_idx: usize,
    end_idx: usize,
) -> Option<RecordFieldTypeUnionSegment> {
    if start_idx >= end_idx {
        return None;
    }
    let colon_idx = top_level_colon_in_range(tokens, angle_roles, start_idx, end_idx);
    let pipe_indices = colon_idx
        .map(|idx| top_level_pipes_in_range(tokens, angle_roles, idx, end_idx))
        .unwrap_or_default();
    Some(RecordFieldTypeUnionSegment {
        start_idx,
        colon_idx,
        end_idx,
        pipe_indices,
    })
}

fn top_level_colon_in_range(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    start_idx: usize,
    end_idx: usize,
) -> Option<usize> {
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in start_idx..end_idx {
        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            TokenKind::Colon
                if paren_depth == 0
                    && bracket_depth == 0
                    && brace_depth == 0
                    && angle_depth == 0 =>
            {
                return Some(idx);
            }
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    None
}

fn plausible_parameter_list_follow(tokens: &[&Token], close_idx: usize) -> bool {
    matches!(
        tokens.get(close_idx + 1).map(|token| token.kind),
        Some(
            TokenKind::Colon
                | TokenKind::FatArrow
                | TokenKind::LBrace
                | TokenKind::Semi
                | TokenKind::Kw(Keyword::Async)
        )
    )
}

fn top_level_param_segment_end(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    colon_idx: usize,
    close_idx: usize,
) -> usize {
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in colon_idx + 1..close_idx {
        if paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && angle_depth == 0
            && tokens[idx].kind == TokenKind::Comma
        {
            return idx;
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    close_idx
}

fn find_generic_bound_wrap(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
) -> Option<GenericBoundWrap> {
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for (idx, token) in tokens.iter().enumerate() {
        if paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && angle_depth > 0
            && token.kind == TokenKind::Colon
        {
            if let Some(bounds) =
                find_generic_bound_wrap_from(tokens, angle_roles, idx, angle_depth)
            {
                return Some(bounds);
            }
        }

        match token.kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    None
}

fn find_generic_parameter_list_bound_wrap(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
) -> Option<GenericParameterListBoundWrap> {
    tokens.iter().enumerate().find_map(|(idx, token)| {
        (token.kind == TokenKind::Lt && angle_roles[idx] == AngleRole::GenericOpen)
            .then(|| find_generic_parameter_list_bound_wrap_from(tokens, angle_roles, idx))
            .flatten()
    })
}

fn find_generic_parameter_list_bound_wrap_from(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    open_idx: usize,
) -> Option<GenericParameterListBoundWrap> {
    if tokens.get(open_idx).map(|token| token.kind) != Some(TokenKind::Lt)
        || angle_roles.get(open_idx).copied() != Some(AngleRole::GenericOpen)
    {
        return None;
    }
    let close_idx = matching_generic_close(tokens, angle_roles, open_idx)?;
    if tokens[close_idx].kind == TokenKind::Shr {
        return None;
    }
    let segments =
        top_level_generic_parameter_bound_segments(tokens, angle_roles, open_idx, close_idx);
    if segments.len() < 2
        || segments
            .iter()
            .all(|segment| segment.plus_indices.is_empty())
    {
        return None;
    }
    Some(GenericParameterListBoundWrap {
        open_idx,
        close_idx,
        segments,
    })
}

fn matching_generic_close(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    open_idx: usize,
) -> Option<usize> {
    if angle_roles.get(open_idx).copied() != Some(AngleRole::GenericOpen) {
        return None;
    }
    let mut depth = 0usize;
    for idx in open_idx + 1..tokens.len() {
        match angle_roles[idx] {
            AngleRole::GenericOpen => depth += 1,
            AngleRole::GenericClose if depth == 0 => return Some(idx),
            AngleRole::GenericClose => depth = depth.saturating_sub(1),
            AngleRole::GenericCloseClose if depth <= 1 => return Some(idx),
            AngleRole::GenericCloseClose => depth = depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }
    None
}

fn top_level_generic_parameter_bound_segments(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    open_idx: usize,
    close_idx: usize,
) -> Vec<GenericParameterSegmentBoundWrap> {
    let mut segments = Vec::new();
    let mut segment_start = open_idx + 1;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in open_idx + 1..=close_idx {
        let is_boundary = idx == close_idx
            || (paren_depth == 0
                && bracket_depth == 0
                && brace_depth == 0
                && angle_depth == 0
                && tokens[idx].kind == TokenKind::Comma);
        if is_boundary {
            if let Some(segment) =
                generic_parameter_bound_segment(tokens, angle_roles, segment_start, idx)
            {
                segments.push(segment);
            }
            segment_start = idx + 1;
            continue;
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    segments
}

fn generic_parameter_bound_segment(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    start_idx: usize,
    end_idx: usize,
) -> Option<GenericParameterSegmentBoundWrap> {
    if start_idx >= end_idx {
        return None;
    }
    let colon_idx = top_level_colon_in_range(tokens, angle_roles, start_idx, end_idx);
    let plus_indices = colon_idx
        .map(|idx| top_level_pluses_in_range(tokens, angle_roles, idx, end_idx))
        .unwrap_or_default();
    Some(GenericParameterSegmentBoundWrap {
        start_idx,
        colon_idx,
        end_idx,
        plus_indices,
    })
}

fn find_generic_bound_wrap_from(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    colon_idx: usize,
    bound_angle_depth: usize,
) -> Option<GenericBoundWrap> {
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = bound_angle_depth;
    let mut end_idx = None;

    for idx in colon_idx + 1..tokens.len() {
        if paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && angle_depth == bound_angle_depth
        {
            if tokens[idx].kind == TokenKind::Comma {
                return None;
            }
            if angle_roles[idx].is_generic_close() {
                end_idx = Some(idx);
                break;
            }
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    let end_idx = end_idx?;
    let plus_indices = top_level_pluses_in_range(tokens, angle_roles, colon_idx, end_idx);
    if plus_indices.is_empty() {
        return None;
    }
    Some(GenericBoundWrap {
        colon_idx,
        end_idx,
        plus_indices,
    })
}

fn find_interface_member_wrap(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
) -> Option<InterfaceMemberWrap> {
    tokens.iter().enumerate().find_map(|(idx, token)| {
        matches!(
            token.kind,
            TokenKind::Kw(Keyword::Interface) | TokenKind::Kw(Keyword::Extend)
        )
        .then(|| find_interface_member_wrap_from(tokens, angle_roles, idx))
        .flatten()
    })
}

fn find_interface_member_wrap_from(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    header_idx: usize,
) -> Option<InterfaceMemberWrap> {
    if !matches!(
        tokens.get(header_idx).map(|token| token.kind),
        Some(TokenKind::Kw(Keyword::Interface) | TokenKind::Kw(Keyword::Extend))
    ) {
        return None;
    }

    let mut lbrace_idx = None;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in header_idx + 1..tokens.len() {
        if paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && angle_depth == 0
            && tokens[idx].kind == TokenKind::LBrace
        {
            lbrace_idx = Some(idx);
            break;
        }

        if paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && angle_depth == 0
            && matches!(
                tokens[idx].kind,
                TokenKind::Semi | TokenKind::Eq | TokenKind::FatArrow
            )
        {
            return None;
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    let lbrace_idx = lbrace_idx?;
    let rbrace_idx = matching_brace_close(tokens, lbrace_idx)?;
    let member_ranges =
        top_level_interface_member_ranges(tokens, angle_roles, lbrace_idx, rbrace_idx);
    if member_ranges.len() < 2 {
        return None;
    }
    Some(InterfaceMemberWrap {
        lbrace_idx,
        rbrace_idx,
        member_ranges,
    })
}

fn find_function_return_union_wrap_from(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    function_idx: usize,
) -> Option<FunctionReturnUnionWrap> {
    if !matches!(
        tokens.get(function_idx).map(|token| token.kind),
        Some(TokenKind::Kw(Keyword::Function))
    ) {
        return None;
    }

    let mut colon_idx = None;
    let mut end_idx = None;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;
    let mut saw_param_close = false;

    for idx in function_idx + 1..tokens.len() {
        if paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && angle_depth == 0
            && matches!(
                tokens[idx].kind,
                TokenKind::LBrace | TokenKind::Semi | TokenKind::Kw(Keyword::Async)
            )
        {
            end_idx = Some(idx);
            break;
        }

        if saw_param_close
            && paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && angle_depth == 0
            && tokens[idx].kind == TokenKind::Colon
        {
            colon_idx = Some(idx);
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => {
                paren_depth = paren_depth.saturating_sub(1);
                if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 && angle_depth == 0 {
                    saw_param_close = true;
                }
            }
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            TokenKind::Eq | TokenKind::FatArrow
                if paren_depth == 0
                    && bracket_depth == 0
                    && brace_depth == 0
                    && angle_depth == 0 =>
            {
                return None;
            }
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    let colon_idx = colon_idx?;
    let end_idx = end_idx?;
    if colon_idx >= end_idx {
        return None;
    }
    let pipe_indices = top_level_pipes_in_range(tokens, angle_roles, colon_idx, end_idx);
    if pipe_indices.is_empty() {
        return None;
    }
    Some(FunctionReturnUnionWrap {
        colon_idx,
        end_idx,
        pipe_indices,
    })
}

fn find_interface_bound_wrap(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
) -> Option<InterfaceBoundWrap> {
    find_interface_bound_wrap_from(tokens, angle_roles, 0)
}

fn find_interface_bound_wrap_from(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    start_idx: usize,
) -> Option<InterfaceBoundWrap> {
    if !matches!(
        tokens.get(start_idx).map(|token| token.kind),
        Some(TokenKind::Kw(Keyword::Interface) | TokenKind::Kw(Keyword::Extend))
    ) {
        return None;
    }

    let mut colon_idx = None;
    let mut lbrace_idx = None;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in start_idx + 1..tokens.len() {
        if paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && angle_depth == 0
            && tokens[idx].kind == TokenKind::Colon
        {
            colon_idx = Some(idx);
        }

        if paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && angle_depth == 0
            && tokens[idx].kind == TokenKind::LBrace
        {
            lbrace_idx = Some(idx);
            break;
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            TokenKind::Semi | TokenKind::Eq | TokenKind::FatArrow
                if paren_depth == 0
                    && bracket_depth == 0
                    && brace_depth == 0
                    && angle_depth == 0 =>
            {
                return None;
            }
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    let colon_idx = colon_idx?;
    let lbrace_idx = lbrace_idx?;
    if colon_idx >= lbrace_idx {
        return None;
    }
    let plus_indices = top_level_pluses_in_range(tokens, angle_roles, colon_idx, lbrace_idx);
    if plus_indices.is_empty() {
        return None;
    }
    Some(InterfaceBoundWrap {
        colon_idx,
        lbrace_idx,
        plus_indices,
    })
}

fn find_var_initializer_wrap(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
) -> Option<VarInitializerWrap> {
    if tokens.first().map(|token| token.kind) != Some(TokenKind::Kw(Keyword::Var)) {
        return None;
    }

    let mut eq_idx = None;
    let mut final_semi_idx = None;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in 1..tokens.len() {
        if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 && angle_depth == 0 {
            match tokens[idx].kind {
                TokenKind::Eq => {
                    if eq_idx.is_some() {
                        return None;
                    }
                    eq_idx = Some(idx);
                }
                TokenKind::Semi => {
                    if idx + 1 != tokens.len() {
                        return None;
                    }
                    final_semi_idx = Some(idx);
                }
                _ => {}
            }
        }
        if eq_idx.is_some() && var_initializer_rhs_operator(tokens[idx].kind, angle_roles[idx]) {
            return None;
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    let eq_idx = eq_idx?;
    let final_semi_idx = final_semi_idx?;
    (eq_idx + 1 < final_semi_idx).then_some(VarInitializerWrap { eq_idx })
}

fn var_initializer_rhs_operator(kind: TokenKind, angle_role: AngleRole) -> bool {
    match kind {
        TokenKind::Plus
        | TokenKind::Minus
        | TokenKind::Star
        | TokenKind::Slash
        | TokenKind::Percent
        | TokenKind::Amp
        | TokenKind::Caret
        | TokenKind::Pipe
        | TokenKind::AmpAmp
        | TokenKind::PipePipe
        | TokenKind::FatArrow
        | TokenKind::Kw(Keyword::As)
        | TokenKind::Kw(Keyword::Is) => true,
        TokenKind::EqEq
        | TokenKind::BangEq
        | TokenKind::Lt
        | TokenKind::LtEq
        | TokenKind::Gt
        | TokenKind::GtEq
        | TokenKind::Shl
        | TokenKind::Shr => angle_role == AngleRole::None,
        _ => false,
    }
}

fn find_return_expr_wrap(tokens: &[&Token], angle_roles: &[AngleRole]) -> Option<ReturnExprWrap> {
    if tokens.first().map(|token| token.kind) != Some(TokenKind::Kw(Keyword::Return)) {
        return None;
    }
    if tokens.len() < 2 {
        return None;
    }

    let mut final_semi_idx = None;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in 1..tokens.len() {
        if paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && angle_depth == 0
            && tokens[idx].kind == TokenKind::Semi
        {
            if idx + 1 != tokens.len() {
                return None;
            }
            final_semi_idx = Some(idx);
        }
        if return_expr_rhs_operator(tokens[idx].kind, angle_roles[idx]) {
            return None;
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    let end_idx = final_semi_idx.unwrap_or(tokens.len());
    (end_idx > 1).then_some(ReturnExprWrap { value_start_idx: 1 })
}

fn return_expr_rhs_operator(kind: TokenKind, angle_role: AngleRole) -> bool {
    var_initializer_rhs_operator(kind, angle_role)
}

fn find_break_expr_wrap(tokens: &[&Token], angle_roles: &[AngleRole]) -> Option<BreakExprWrap> {
    if tokens.first().map(|token| token.kind) != Some(TokenKind::Kw(Keyword::Break)) {
        return None;
    }
    if tokens.len() < 2 {
        return None;
    }

    let mut final_semi_idx = None;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in 1..tokens.len() {
        if paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && angle_depth == 0
            && tokens[idx].kind == TokenKind::Semi
        {
            if idx + 1 != tokens.len() {
                return None;
            }
            final_semi_idx = Some(idx);
        }
        if break_expr_rhs_operator(tokens[idx].kind, angle_roles[idx]) {
            return None;
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    let end_idx = final_semi_idx.unwrap_or(tokens.len());
    (end_idx > 1).then_some(BreakExprWrap { value_start_idx: 1 })
}

fn break_expr_rhs_operator(kind: TokenKind, angle_role: AngleRole) -> bool {
    var_initializer_rhs_operator(kind, angle_role)
}

fn find_await_spawn_expr_wrap(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
) -> Option<AwaitSpawnExprWrap> {
    if !matches!(
        tokens.first().map(|token| token.kind),
        Some(TokenKind::Kw(Keyword::Await | Keyword::Spawn))
    ) {
        return None;
    }
    if tokens.len() < 2 || tokens[1].kind == TokenKind::LBrace {
        return None;
    }

    let mut final_semi_idx = None;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in 1..tokens.len() {
        if paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && angle_depth == 0
            && tokens[idx].kind == TokenKind::Semi
        {
            if idx + 1 != tokens.len() {
                return None;
            }
            final_semi_idx = Some(idx);
        }
        if await_spawn_expr_rhs_operator(tokens[idx].kind, angle_roles[idx]) {
            return None;
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    let end_idx = final_semi_idx.unwrap_or(tokens.len());
    (end_idx > 1).then_some(AwaitSpawnExprWrap {
        operand_start_idx: 1,
    })
}

fn await_spawn_expr_rhs_operator(kind: TokenKind, angle_role: AngleRole) -> bool {
    var_initializer_rhs_operator(kind, angle_role)
}

fn find_assignment_expr_wrap(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
) -> Option<AssignmentExprWrap> {
    if matches!(
        tokens.first().map(|token| token.kind),
        Some(TokenKind::Kw(
            Keyword::Var
                | Keyword::Function
                | Keyword::Struct
                | Keyword::Interface
                | Keyword::Type
                | Keyword::Mod
                | Keyword::Extend
                | Keyword::Extern
                | Keyword::Import
                | Keyword::Pub
                | Keyword::Return
        ))
    ) {
        return None;
    }

    let mut eq_idx = None;
    let mut final_semi_idx = None;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in 0..tokens.len() {
        if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 && angle_depth == 0 {
            match tokens[idx].kind {
                TokenKind::Eq => {
                    if eq_idx.is_some() {
                        return None;
                    }
                    eq_idx = Some(idx);
                }
                TokenKind::Semi => {
                    if idx + 1 != tokens.len() {
                        return None;
                    }
                    final_semi_idx = Some(idx);
                }
                _ => {}
            }
        }
        if eq_idx.is_some() && assignment_expr_rhs_operator(tokens[idx].kind, angle_roles[idx]) {
            return None;
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    let eq_idx = eq_idx?;
    let final_semi_idx = final_semi_idx?;
    (eq_idx > 0 && eq_idx + 1 < final_semi_idx).then_some(AssignmentExprWrap { eq_idx })
}

fn assignment_expr_rhs_operator(kind: TokenKind, angle_role: AngleRole) -> bool {
    var_initializer_rhs_operator(kind, angle_role)
}

fn find_for_iterator_wrap(tokens: &[&Token], angle_roles: &[AngleRole]) -> Option<ForIteratorWrap> {
    if tokens.first().map(|token| token.kind) != Some(TokenKind::Kw(Keyword::For)) {
        return None;
    }

    let mut in_idx = None;
    let mut lbrace_idx = None;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in 1..tokens.len() {
        if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 && angle_depth == 0 {
            match tokens[idx].kind {
                TokenKind::Kw(Keyword::In) => {
                    if in_idx.is_some() {
                        return None;
                    }
                    in_idx = Some(idx);
                }
                TokenKind::LBrace => {
                    lbrace_idx = Some(idx);
                    break;
                }
                _ => {}
            }
        }
        if in_idx.is_some() && for_iterator_operator(tokens[idx].kind, angle_roles[idx]) {
            return None;
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    let in_idx = in_idx?;
    let lbrace_idx = lbrace_idx?;
    (in_idx > 1 && in_idx + 1 < lbrace_idx).then_some(ForIteratorWrap { in_idx, lbrace_idx })
}

fn for_iterator_operator(kind: TokenKind, angle_role: AngleRole) -> bool {
    var_initializer_rhs_operator(kind, angle_role)
}

fn find_for_body_wrap(tokens: &[&Token], angle_roles: &[AngleRole]) -> Option<ForBodyWrap> {
    let iter = find_for_iterator_wrap(tokens, angle_roles)?;
    if iter.lbrace_idx <= iter.in_idx + 1
        || tokens[iter.lbrace_idx].span.range().start > MAX_LINE_LENGTH / 2
    {
        return None;
    }
    find_for_body_wrap_from(tokens, angle_roles, iter.lbrace_idx)
}

fn find_for_body_wrap_from(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    open_idx: usize,
) -> Option<ForBodyWrap> {
    let close_idx = matching_brace_close(tokens, open_idx)?;
    if open_idx + 1 >= close_idx {
        return None;
    }
    if close_idx + 1 != tokens.len() {
        return None;
    }
    for body_idx in (open_idx + 1)..close_idx {
        match tokens[body_idx].kind {
            TokenKind::Colon
            | TokenKind::Comma
            | TokenKind::DotDot
            | TokenKind::FatArrow
            | TokenKind::LBrace
            | TokenKind::RBrace => return None,
            kind if arrow_closure_body_operator(kind, angle_roles[body_idx]) => return None,
            _ => {}
        }
    }

    Some(ForBodyWrap {
        lbrace_idx: open_idx,
    })
}

fn find_while_condition_wrap(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
) -> Option<WhileConditionWrap> {
    if tokens.first().map(|token| token.kind) != Some(TokenKind::Kw(Keyword::While)) {
        return None;
    }

    let mut lbrace_idx = None;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in 1..tokens.len() {
        if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 && angle_depth == 0 {
            if tokens[idx].kind == TokenKind::LBrace {
                lbrace_idx = Some(idx);
                break;
            }
        }
        if while_condition_operator(tokens[idx].kind, angle_roles[idx]) {
            return None;
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    let lbrace_idx = lbrace_idx?;
    (lbrace_idx > 1).then_some(WhileConditionWrap {
        condition_start_idx: 1,
        lbrace_idx,
    })
}

fn while_condition_operator(kind: TokenKind, angle_role: AngleRole) -> bool {
    var_initializer_rhs_operator(kind, angle_role)
}

fn find_while_body_wrap(tokens: &[&Token], angle_roles: &[AngleRole]) -> Option<WhileBodyWrap> {
    if tokens.first().map(|token| token.kind) != Some(TokenKind::Kw(Keyword::While)) {
        return None;
    }

    let mut lbrace_idx = None;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in 1..tokens.len() {
        if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 && angle_depth == 0 {
            if tokens[idx].kind == TokenKind::LBrace {
                lbrace_idx = Some(idx);
                break;
            }
        }
        if while_condition_operator(tokens[idx].kind, angle_roles[idx]) {
            return None;
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    let lbrace_idx = lbrace_idx?;
    if lbrace_idx <= 1 || tokens[lbrace_idx].span.range().start > MAX_LINE_LENGTH / 2 {
        return None;
    }
    find_while_body_wrap_from(tokens, angle_roles, lbrace_idx)
}

fn find_while_body_wrap_from(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    open_idx: usize,
) -> Option<WhileBodyWrap> {
    let close_idx = matching_brace_close(tokens, open_idx)?;
    if open_idx + 1 >= close_idx {
        return None;
    }
    if close_idx + 1 != tokens.len() {
        return None;
    }
    for body_idx in (open_idx + 1)..close_idx {
        match tokens[body_idx].kind {
            TokenKind::Colon
            | TokenKind::Comma
            | TokenKind::DotDot
            | TokenKind::FatArrow
            | TokenKind::LBrace
            | TokenKind::RBrace => return None,
            kind if arrow_closure_body_operator(kind, angle_roles[body_idx]) => return None,
            _ => {}
        }
    }

    Some(WhileBodyWrap {
        lbrace_idx: open_idx,
    })
}

fn find_if_condition_wrap(tokens: &[&Token], angle_roles: &[AngleRole]) -> Option<IfConditionWrap> {
    let condition_start_idx =
        if tokens.first().map(|token| token.kind) == Some(TokenKind::Kw(Keyword::If)) {
            1
        } else if tokens.len() >= 2
            && tokens[0].kind == TokenKind::Kw(Keyword::Else)
            && tokens[1].kind == TokenKind::Kw(Keyword::If)
        {
            2
        } else if tokens.len() >= 3
            && tokens[0].kind == TokenKind::RBrace
            && tokens[1].kind == TokenKind::Kw(Keyword::Else)
            && tokens[2].kind == TokenKind::Kw(Keyword::If)
        {
            3
        } else {
            return None;
        };

    let mut lbrace_idx = None;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in condition_start_idx..tokens.len() {
        if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 && angle_depth == 0 {
            if tokens[idx].kind == TokenKind::LBrace {
                lbrace_idx = Some(idx);
                break;
            }
        }
        if if_condition_operator(tokens[idx].kind, angle_roles[idx]) {
            return None;
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    let lbrace_idx = lbrace_idx?;
    (condition_start_idx < lbrace_idx).then_some(IfConditionWrap {
        condition_start_idx,
        lbrace_idx,
    })
}

fn if_condition_operator(kind: TokenKind, angle_role: AngleRole) -> bool {
    var_initializer_rhs_operator(kind, angle_role)
}

fn find_if_body_wrap(tokens: &[&Token], angle_roles: &[AngleRole]) -> Option<IfBodyWrap> {
    if tokens.first().map(|token| token.kind) != Some(TokenKind::Kw(Keyword::If)) {
        return None;
    }

    let mut lbrace_idx = None;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in 1..tokens.len() {
        if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 && angle_depth == 0 {
            if tokens[idx].kind == TokenKind::LBrace {
                lbrace_idx = Some(idx);
                break;
            }
        }
        if if_condition_operator(tokens[idx].kind, angle_roles[idx]) {
            return None;
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    let lbrace_idx = lbrace_idx?;
    if lbrace_idx <= 1 || tokens[lbrace_idx].span.range().start > MAX_LINE_LENGTH / 2 {
        return None;
    }
    find_if_body_wrap_from(tokens, angle_roles, lbrace_idx)
}

fn find_if_body_wrap_from(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    open_idx: usize,
) -> Option<IfBodyWrap> {
    let close_idx = matching_brace_close(tokens, open_idx)?;
    if open_idx + 1 >= close_idx {
        return None;
    }
    if close_idx + 1 != tokens.len() {
        return None;
    }
    for body_idx in (open_idx + 1)..close_idx {
        match tokens[body_idx].kind {
            TokenKind::Colon
            | TokenKind::Comma
            | TokenKind::DotDot
            | TokenKind::FatArrow
            | TokenKind::LBrace
            | TokenKind::RBrace => return None,
            kind if arrow_closure_body_operator(kind, angle_roles[body_idx]) => return None,
            _ => {}
        }
    }

    Some(IfBodyWrap {
        lbrace_idx: open_idx,
    })
}

fn find_match_scrutinee_wrap(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
) -> Option<MatchScrutineeWrap> {
    let mut match_idx = None;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in 0..tokens.len() {
        if paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && angle_depth == 0
            && tokens[idx].kind == TokenKind::Kw(Keyword::Match)
        {
            match_idx = Some(idx);
            break;
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    let match_idx = match_idx?;
    let mut lbrace_idx = None;
    paren_depth = 0;
    bracket_depth = 0;
    brace_depth = 0;
    angle_depth = 0;

    for idx in (match_idx + 1)..tokens.len() {
        if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 && angle_depth == 0 {
            if tokens[idx].kind == TokenKind::LBrace {
                lbrace_idx = Some(idx);
                break;
            }
        }
        if match_scrutinee_operator(tokens[idx].kind, angle_roles[idx]) {
            return None;
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    let lbrace_idx = lbrace_idx?;
    (match_idx + 1 < lbrace_idx).then_some(MatchScrutineeWrap {
        match_idx,
        lbrace_idx,
    })
}

fn match_scrutinee_operator(kind: TokenKind, angle_role: AngleRole) -> bool {
    var_initializer_rhs_operator(kind, angle_role)
}

fn find_match_guard_wrap(tokens: &[&Token], angle_roles: &[AngleRole]) -> Option<MatchGuardWrap> {
    let mut if_idx = None;
    let mut fat_arrow_idx = None;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in 0..tokens.len() {
        if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 && angle_depth == 0 {
            match tokens[idx].kind {
                TokenKind::Kw(Keyword::If) => {
                    if if_idx.is_some() {
                        return None;
                    }
                    if_idx = Some(idx);
                }
                TokenKind::FatArrow => {
                    fat_arrow_idx = Some(idx);
                    break;
                }
                _ => {}
            }
        }
        if if_idx.is_some() && match_guard_operator(tokens[idx].kind, angle_roles[idx]) {
            return None;
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    let if_idx = if_idx?;
    let fat_arrow_idx = fat_arrow_idx?;
    (if_idx > 0 && if_idx + 1 < fat_arrow_idx).then_some(MatchGuardWrap {
        if_idx,
        fat_arrow_idx,
    })
}

fn match_guard_operator(kind: TokenKind, angle_role: AngleRole) -> bool {
    var_initializer_rhs_operator(kind, angle_role)
}

fn find_match_arm_body_wrap(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
) -> Option<MatchArmBodyWrap> {
    let mut fat_arrow_idx = None;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in 0..tokens.len() {
        if paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && angle_depth == 0
            && tokens[idx].kind == TokenKind::FatArrow
        {
            if fat_arrow_idx.is_some() {
                return None;
            }
            fat_arrow_idx = Some(idx);
        }

        if fat_arrow_idx.is_some_and(|arrow_idx| idx > arrow_idx)
            && match_arm_body_operator(tokens[idx].kind, angle_roles[idx])
        {
            return None;
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    let fat_arrow_idx = fat_arrow_idx?;
    if !plausible_match_arm_body_prefix(tokens, angle_roles, fat_arrow_idx) {
        return None;
    }
    (fat_arrow_idx > 0 && fat_arrow_idx + 1 < tokens.len())
        .then_some(MatchArmBodyWrap { fat_arrow_idx })
}

fn match_arm_body_operator(kind: TokenKind, angle_role: AngleRole) -> bool {
    var_initializer_rhs_operator(kind, angle_role)
}

fn plausible_match_arm_body_prefix(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    fat_arrow_idx: usize,
) -> bool {
    if matches!(
        tokens.first().map(|token| token.kind),
        Some(TokenKind::LParen | TokenKind::Kw(Keyword::Async))
    ) {
        return false;
    }

    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in 0..fat_arrow_idx {
        let top_level =
            paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 && angle_depth == 0;
        if top_level {
            match tokens[idx].kind {
                TokenKind::Eq | TokenKind::Semi | TokenKind::Kw(Keyword::Async | Keyword::If) => {
                    return false;
                }
                _ => {}
            }
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    true
}

fn find_arrow_closure_body_wrap(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
) -> Option<ArrowClosureBodyWrap> {
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in 0..tokens.len() {
        let top_level =
            paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 && angle_depth == 0;
        if top_level
            && tokens[idx].kind == TokenKind::LParen
            && plausible_arrow_closure_start(tokens, idx)
        {
            if let Some(body) = find_arrow_closure_body_wrap_from(tokens, angle_roles, idx) {
                return Some(body);
            }
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    None
}

fn plausible_arrow_closure_start(tokens: &[&Token], open_idx: usize) -> bool {
    if open_idx == 0 {
        return true;
    }
    matches!(
        tokens[open_idx - 1].kind,
        TokenKind::Eq
            | TokenKind::Comma
            | TokenKind::LParen
            | TokenKind::LBracket
            | TokenKind::LBrace
            | TokenKind::Kw(Keyword::Return)
    )
}

fn find_arrow_closure_body_wrap_from(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    open_idx: usize,
) -> Option<ArrowClosureBodyWrap> {
    let close_idx = matching_paren_close(tokens, open_idx)?;
    let mut idx = close_idx + 1;
    if tokens.get(idx).map(|token| token.kind) == Some(TokenKind::Colon) {
        idx += 1;
        idx = skip_arrow_closure_return_type(tokens, angle_roles, idx)?;
    }
    if tokens.get(idx).map(|token| token.kind) == Some(TokenKind::Kw(Keyword::Async)) {
        idx += 1;
    }
    if tokens.get(idx).map(|token| token.kind) != Some(TokenKind::FatArrow) {
        return None;
    }
    let fat_arrow_idx = idx;
    if fat_arrow_idx + 1 >= tokens.len() {
        return None;
    }
    for body_idx in (fat_arrow_idx + 1)..tokens.len() {
        if arrow_closure_body_operator(tokens[body_idx].kind, angle_roles[body_idx]) {
            return None;
        }
    }

    Some(ArrowClosureBodyWrap { fat_arrow_idx })
}

fn skip_arrow_closure_return_type(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    start_idx: usize,
) -> Option<usize> {
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in start_idx..tokens.len() {
        if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 && angle_depth == 0 {
            match tokens[idx].kind {
                TokenKind::Kw(Keyword::Async) | TokenKind::FatArrow => return Some(idx),
                TokenKind::Eq | TokenKind::Semi | TokenKind::LBrace | TokenKind::RBrace => {
                    return None;
                }
                _ => {}
            }
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    None
}

fn arrow_closure_body_operator(kind: TokenKind, angle_role: AngleRole) -> bool {
    var_initializer_rhs_operator(kind, angle_role)
}

fn find_trailing_closure_body_wrap(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
) -> Option<TrailingClosureBodyWrap> {
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in 0..tokens.len() {
        let top_level =
            paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 && angle_depth == 0;
        if top_level
            && tokens[idx].kind == TokenKind::LBrace
            && plausible_trailing_closure_open(tokens, angle_roles, idx)
        {
            if let Some(body) = find_trailing_closure_body_wrap_from(tokens, angle_roles, idx) {
                return Some(body);
            }
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    None
}

fn plausible_trailing_closure_open(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    open_idx: usize,
) -> bool {
    if open_idx == 0 {
        return false;
    }
    if matches!(
        tokens[open_idx - 1].kind,
        TokenKind::RParen | TokenKind::RBracket
    ) {
        return true;
    }

    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in 0..open_idx {
        let top_level =
            paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 && angle_depth == 0;
        if top_level && tokens[idx].kind == TokenKind::Dot {
            return true;
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    false
}

fn find_trailing_closure_body_wrap_from(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    open_idx: usize,
) -> Option<TrailingClosureBodyWrap> {
    let close_idx = matching_brace_close(tokens, open_idx)?;
    let mut idx = open_idx + 1;
    if idx >= close_idx {
        return None;
    }

    while idx < close_idx
        && !matches!(
            tokens[idx].kind,
            TokenKind::Kw(Keyword::Async) | TokenKind::FatArrow
        )
    {
        if tokens[idx].kind != TokenKind::Ident {
            return None;
        }
        idx += 1;
        if idx < close_idx && tokens[idx].kind == TokenKind::Colon {
            idx += 1;
            idx = skip_trailing_closure_param_type(tokens, angle_roles, idx, close_idx)?;
        }
        if idx < close_idx && tokens[idx].kind == TokenKind::Comma {
            idx += 1;
            continue;
        }
        break;
    }

    if tokens.get(idx).map(|token| token.kind) == Some(TokenKind::Kw(Keyword::Async)) {
        idx += 1;
    }
    if tokens.get(idx).map(|token| token.kind) != Some(TokenKind::FatArrow) {
        return None;
    }
    let fat_arrow_idx = idx;
    if fat_arrow_idx + 1 >= close_idx {
        return None;
    }
    for body_idx in (fat_arrow_idx + 1)..close_idx {
        if arrow_closure_body_operator(tokens[body_idx].kind, angle_roles[body_idx]) {
            return None;
        }
    }

    Some(TrailingClosureBodyWrap { fat_arrow_idx })
}

fn skip_trailing_closure_param_type(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    start_idx: usize,
    close_idx: usize,
) -> Option<usize> {
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in start_idx..close_idx {
        if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 && angle_depth == 0 {
            match tokens[idx].kind {
                TokenKind::Comma | TokenKind::Kw(Keyword::Async) | TokenKind::FatArrow => {
                    return Some(idx);
                }
                TokenKind::Eq | TokenKind::Semi => return None,
                _ => {}
            }
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    None
}

fn find_implicit_trailing_closure_body_wrap(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
) -> Option<ImplicitTrailingClosureBodyWrap> {
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in 0..tokens.len() {
        let top_level =
            paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 && angle_depth == 0;
        if top_level
            && tokens[idx].kind == TokenKind::LBrace
            && plausible_trailing_closure_open(tokens, angle_roles, idx)
        {
            if let Some(body) =
                find_implicit_trailing_closure_body_wrap_from(tokens, angle_roles, idx)
            {
                return Some(body);
            }
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    None
}

fn find_implicit_trailing_closure_body_wrap_from(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    open_idx: usize,
) -> Option<ImplicitTrailingClosureBodyWrap> {
    let close_idx = matching_brace_close(tokens, open_idx)?;
    if open_idx + 1 >= close_idx {
        return None;
    }
    for body_idx in (open_idx + 1)..close_idx {
        match tokens[body_idx].kind {
            TokenKind::FatArrow | TokenKind::LBrace | TokenKind::RBrace => return None,
            kind if arrow_closure_body_operator(kind, angle_roles[body_idx]) => return None,
            _ => {}
        }
    }

    Some(ImplicitTrailingClosureBodyWrap {
        lbrace_idx: open_idx,
    })
}

fn find_async_block_body_wrap(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
) -> Option<AsyncBlockBodyWrap> {
    for idx in 0..tokens.len() {
        if tokens[idx].kind == TokenKind::Kw(Keyword::Async)
            && tokens.get(idx + 1).map(|token| token.kind) == Some(TokenKind::LBrace)
            && plausible_async_block_start(tokens, idx)
        {
            if let Some(body) = find_async_block_body_wrap_from(tokens, angle_roles, idx + 1) {
                return Some(body);
            }
        }
    }

    None
}

fn plausible_async_block_start(tokens: &[&Token], async_idx: usize) -> bool {
    if async_idx == 0 {
        return true;
    }
    matches!(
        tokens[async_idx - 1].kind,
        TokenKind::Eq
            | TokenKind::Comma
            | TokenKind::LParen
            | TokenKind::LBracket
            | TokenKind::LBrace
            | TokenKind::Kw(Keyword::Return | Keyword::Await | Keyword::Spawn)
    )
}

fn find_async_block_body_wrap_from(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    open_idx: usize,
) -> Option<AsyncBlockBodyWrap> {
    let close_idx = matching_brace_close(tokens, open_idx)?;
    if open_idx + 1 >= close_idx {
        return None;
    }
    for body_idx in (open_idx + 1)..close_idx {
        match tokens[body_idx].kind {
            TokenKind::FatArrow | TokenKind::LBrace | TokenKind::RBrace => return None,
            kind if arrow_closure_body_operator(kind, angle_roles[body_idx]) => return None,
            _ => {}
        }
    }

    Some(AsyncBlockBodyWrap {
        lbrace_idx: open_idx,
    })
}

fn find_block_expression_body_wrap(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
) -> Option<BlockExpressionBodyWrap> {
    for idx in 0..tokens.len() {
        if tokens[idx].kind == TokenKind::LBrace
            && plausible_block_expression_open(tokens, idx)
            && let Some(body) = find_block_expression_body_wrap_from(tokens, angle_roles, idx)
        {
            return Some(body);
        }
    }

    None
}

fn plausible_block_expression_open(tokens: &[&Token], open_idx: usize) -> bool {
    if open_idx == 0 {
        return true;
    }
    matches!(
        tokens[open_idx - 1].kind,
        TokenKind::Eq
            | TokenKind::Comma
            | TokenKind::LParen
            | TokenKind::LBracket
            | TokenKind::LBrace
            | TokenKind::Kw(Keyword::Return | Keyword::Await | Keyword::Spawn)
    )
}

fn find_block_expression_body_wrap_from(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    open_idx: usize,
) -> Option<BlockExpressionBodyWrap> {
    let close_idx = matching_brace_close(tokens, open_idx)?;
    if open_idx + 1 >= close_idx {
        return None;
    }
    for body_idx in (open_idx + 1)..close_idx {
        match tokens[body_idx].kind {
            TokenKind::Colon
            | TokenKind::Comma
            | TokenKind::DotDot
            | TokenKind::FatArrow
            | TokenKind::LBrace
            | TokenKind::RBrace => return None,
            kind if arrow_closure_body_operator(kind, angle_roles[body_idx]) => return None,
            _ => {}
        }
    }

    Some(BlockExpressionBodyWrap {
        lbrace_idx: open_idx,
    })
}

fn find_macro_block_body_wrap(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
) -> Option<MacroBlockBodyWrap> {
    for idx in 0..tokens.len() {
        if tokens[idx].kind != TokenKind::At
            || tokens.get(idx + 1).map(|token| token.kind) != Some(TokenKind::Ident)
        {
            continue;
        }

        let mut lbrace_idx = idx + 2;
        if tokens.get(lbrace_idx).map(|token| token.kind) == Some(TokenKind::LParen) {
            lbrace_idx = matching_paren_close(tokens, lbrace_idx)? + 1;
        }
        if tokens.get(lbrace_idx).map(|token| token.kind) != Some(TokenKind::LBrace) {
            continue;
        }
        if let Some(body) = find_macro_block_body_wrap_from(tokens, angle_roles, lbrace_idx) {
            return Some(body);
        }
    }

    None
}

fn find_macro_block_body_wrap_from(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    open_idx: usize,
) -> Option<MacroBlockBodyWrap> {
    let close_idx = matching_brace_close(tokens, open_idx)?;
    if open_idx + 1 >= close_idx {
        return None;
    }
    for body_idx in (open_idx + 1)..close_idx {
        match tokens[body_idx].kind {
            TokenKind::Colon
            | TokenKind::Comma
            | TokenKind::DotDot
            | TokenKind::FatArrow
            | TokenKind::LBrace
            | TokenKind::RBrace => return None,
            kind if arrow_closure_body_operator(kind, angle_roles[body_idx]) => return None,
            _ => {}
        }
    }

    Some(MacroBlockBodyWrap {
        lbrace_idx: open_idx,
    })
}

fn find_loop_body_wrap(tokens: &[&Token], angle_roles: &[AngleRole]) -> Option<LoopBodyWrap> {
    if tokens.first().map(|token| token.kind) != Some(TokenKind::Kw(Keyword::Loop)) {
        return None;
    }
    if tokens.get(1).map(|token| token.kind) != Some(TokenKind::LBrace) {
        return None;
    }
    find_loop_body_wrap_from(tokens, angle_roles, 1)
}

fn find_loop_body_wrap_from(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    open_idx: usize,
) -> Option<LoopBodyWrap> {
    let close_idx = matching_brace_close(tokens, open_idx)?;
    if open_idx + 1 >= close_idx {
        return None;
    }
    if close_idx + 1 != tokens.len() {
        return None;
    }
    for body_idx in (open_idx + 1)..close_idx {
        match tokens[body_idx].kind {
            TokenKind::Colon
            | TokenKind::Comma
            | TokenKind::DotDot
            | TokenKind::FatArrow
            | TokenKind::LBrace
            | TokenKind::RBrace => return None,
            kind if arrow_closure_body_operator(kind, angle_roles[body_idx]) => return None,
            _ => {}
        }
    }

    Some(LoopBodyWrap {
        lbrace_idx: open_idx,
    })
}

fn find_else_body_wrap(tokens: &[&Token], angle_roles: &[AngleRole]) -> Option<ElseBodyWrap> {
    let lbrace_idx = if tokens.first().map(|token| token.kind) == Some(TokenKind::Kw(Keyword::Else))
    {
        1
    } else if tokens.len() >= 3
        && tokens[0].kind == TokenKind::RBrace
        && tokens[1].kind == TokenKind::Kw(Keyword::Else)
    {
        2
    } else {
        return None;
    };
    if tokens.get(lbrace_idx).map(|token| token.kind) != Some(TokenKind::LBrace) {
        return None;
    }
    find_else_body_wrap_from(tokens, angle_roles, lbrace_idx)
}

fn find_else_body_wrap_from(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    open_idx: usize,
) -> Option<ElseBodyWrap> {
    let close_idx = matching_brace_close(tokens, open_idx)?;
    if open_idx + 1 >= close_idx {
        return None;
    }
    if close_idx + 1 != tokens.len() {
        return None;
    }
    for body_idx in (open_idx + 1)..close_idx {
        match tokens[body_idx].kind {
            TokenKind::Colon
            | TokenKind::Comma
            | TokenKind::DotDot
            | TokenKind::FatArrow
            | TokenKind::LBrace
            | TokenKind::RBrace => return None,
            kind if arrow_closure_body_operator(kind, angle_roles[body_idx]) => return None,
            _ => {}
        }
    }

    Some(ElseBodyWrap {
        lbrace_idx: open_idx,
    })
}

fn find_test_decl_header_wrap(line: &str, tokens: &[&Token]) -> Option<TestDeclHeaderWrap> {
    if tokens.len() < 4 || tokens.first().map(|token| token.kind) != Some(TokenKind::Ident) {
        return None;
    }
    let keyword_text = line.get(tokens[0].span.range())?;
    if !matches!(keyword_text, "test" | "bench") {
        return None;
    }
    if tokens.get(1).map(|token| token.kind) != Some(TokenKind::StrStart) {
        return None;
    }

    let mut lbrace_idx = None;
    for idx in 2..tokens.len() {
        if tokens[idx].kind == TokenKind::LBrace {
            lbrace_idx = Some(idx);
            break;
        }
    }

    let lbrace_idx = lbrace_idx?;
    (1 < lbrace_idx).then_some(TestDeclHeaderWrap {
        keyword_idx: 0,
        lbrace_idx,
    })
}

fn find_test_decl_body_wrap(
    line: &str,
    tokens: &[&Token],
    angle_roles: &[AngleRole],
) -> Option<TestDeclBodyWrap> {
    let header = find_test_decl_header_wrap(line, tokens)?;
    if tokens[header.lbrace_idx].span.range().start > MAX_LINE_LENGTH / 2 {
        return None;
    }
    find_test_decl_body_wrap_from(tokens, angle_roles, header.lbrace_idx)
}

fn find_test_decl_body_wrap_from(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    open_idx: usize,
) -> Option<TestDeclBodyWrap> {
    let close_idx = matching_brace_close(tokens, open_idx)?;
    if open_idx + 1 >= close_idx {
        return None;
    }
    if close_idx + 1 != tokens.len() {
        return None;
    }
    for body_idx in (open_idx + 1)..close_idx {
        match tokens[body_idx].kind {
            TokenKind::Colon
            | TokenKind::Comma
            | TokenKind::DotDot
            | TokenKind::FatArrow
            | TokenKind::LBrace
            | TokenKind::RBrace => return None,
            kind if arrow_closure_body_operator(kind, angle_roles[body_idx]) => return None,
            _ => {}
        }
    }

    Some(TestDeclBodyWrap {
        lbrace_idx: open_idx,
    })
}

fn find_type_alias_decl_header_wrap(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
) -> Option<TypeAliasDeclHeaderWrap> {
    let type_idx = match tokens.first().map(|token| token.kind) {
        Some(TokenKind::Kw(Keyword::Type)) => 0,
        Some(TokenKind::Kw(Keyword::Pub))
            if tokens.get(1).map(|token| token.kind) == Some(TokenKind::Kw(Keyword::Type)) =>
        {
            1
        }
        _ => return None,
    };
    if tokens.get(type_idx + 1).map(|token| token.kind) != Some(TokenKind::Ident) {
        return None;
    }

    let mut eq_idx = None;
    let mut semi_idx = None;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in (type_idx + 2)..tokens.len() {
        if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 && angle_depth == 0 {
            match tokens[idx].kind {
                TokenKind::Eq => {
                    if eq_idx.is_some() {
                        return None;
                    }
                    eq_idx = Some(idx);
                }
                TokenKind::Semi => {
                    semi_idx = Some(idx);
                    break;
                }
                TokenKind::Pipe if eq_idx.is_some() => return None,
                TokenKind::FatArrow => return None,
                _ => {}
            }
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles.get(idx).copied().unwrap_or(AngleRole::None) {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    let eq_idx = eq_idx?;
    let semi_idx = semi_idx?;
    if semi_idx + 1 != tokens.len() || eq_idx <= type_idx + 1 || eq_idx + 1 >= semi_idx {
        return None;
    }
    Some(TypeAliasDeclHeaderWrap { type_idx })
}

fn find_interface_decl_header_wrap(
    line: &str,
    tokens: &[&Token],
    angle_roles: &[AngleRole],
) -> Option<InterfaceDeclHeaderWrap> {
    let keyword_idx = if token_is_keyword_text(line, tokens, 0, "interface")
        || token_is_keyword_text(line, tokens, 0, "extend")
    {
        0
    } else {
        match tokens.first().map(|token| token.kind) {
            Some(TokenKind::Kw(Keyword::Pub))
                if token_is_keyword_text(line, tokens, 1, "interface") =>
            {
                1
            }
            _ => return None,
        }
    };

    match line.get(tokens[keyword_idx].span.range()) {
        Some("interface") => {
            if tokens.get(keyword_idx + 1).map(|token| token.kind) != Some(TokenKind::Ident) {
                return None;
            }
        }
        Some("extend") => {
            if tokens.get(keyword_idx + 1).is_none() {
                return None;
            }
        }
        _ => return None,
    }

    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;
    let mut lbrace_idx = None;

    for idx in (keyword_idx + 1)..tokens.len() {
        if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 && angle_depth == 0 {
            match tokens[idx].kind {
                TokenKind::LBrace => {
                    lbrace_idx = Some(idx);
                    break;
                }
                TokenKind::Semi | TokenKind::Eq | TokenKind::FatArrow => return None,
                _ => {}
            }
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles.get(idx).copied().unwrap_or(AngleRole::None) {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    let lbrace_idx = lbrace_idx?;
    (keyword_idx + 1 < lbrace_idx).then_some(InterfaceDeclHeaderWrap { keyword_idx })
}

fn token_is_keyword_text(line: &str, tokens: &[&Token], idx: usize, text: &str) -> bool {
    let Some(token) = tokens.get(idx) else {
        return false;
    };
    line.get(token.span.range()) == Some(text)
}

fn find_struct_decl_header_wrap(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
) -> Option<StructDeclHeaderWrap> {
    let struct_idx = match tokens.first().map(|token| token.kind) {
        Some(TokenKind::Kw(Keyword::Struct)) => 0,
        Some(TokenKind::Kw(Keyword::Pub))
            if tokens.get(1).map(|token| token.kind) == Some(TokenKind::Kw(Keyword::Struct)) =>
        {
            1
        }
        Some(TokenKind::Kw(Keyword::Extern))
            if tokens.get(1).map(|token| token.kind) == Some(TokenKind::Kw(Keyword::Struct)) =>
        {
            1
        }
        Some(TokenKind::Kw(Keyword::Pub))
            if tokens.get(1).map(|token| token.kind) == Some(TokenKind::Kw(Keyword::Extern))
                && tokens.get(2).map(|token| token.kind)
                    == Some(TokenKind::Kw(Keyword::Struct)) =>
        {
            2
        }
        _ => return None,
    };
    if tokens.get(struct_idx + 1).map(|token| token.kind) != Some(TokenKind::Ident) {
        return None;
    }

    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;
    let mut saw_decl_end = false;
    let mut has_top_level_field_comma = false;

    for idx in (struct_idx + 2)..tokens.len() {
        if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 && angle_depth == 0 {
            match tokens[idx].kind {
                TokenKind::Semi => {
                    saw_decl_end = idx + 1 == tokens.len();
                    break;
                }
                TokenKind::LParen => {
                    saw_decl_end = true;
                    paren_depth += 1;
                    continue;
                }
                TokenKind::LBrace => {
                    saw_decl_end = true;
                    brace_depth += 1;
                    continue;
                }
                TokenKind::Eq | TokenKind::FatArrow => return None,
                _ => {}
            }
        }

        if ((paren_depth == 1 && brace_depth == 0) || (brace_depth == 1 && paren_depth == 0))
            && bracket_depth == 0
            && angle_depth == 0
            && tokens[idx].kind == TokenKind::Comma
        {
            has_top_level_field_comma = true;
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles.get(idx).copied().unwrap_or(AngleRole::None) {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    if !saw_decl_end || has_top_level_field_comma {
        return None;
    }
    Some(StructDeclHeaderWrap { struct_idx })
}

fn find_function_decl_header_wrap(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
) -> Option<FunctionDeclHeaderWrap> {
    let function_idx = match tokens.first().map(|token| token.kind) {
        Some(TokenKind::Kw(Keyword::Function)) => 0,
        Some(TokenKind::Kw(Keyword::Pub))
            if tokens.get(1).map(|token| token.kind) == Some(TokenKind::Kw(Keyword::Function)) =>
        {
            1
        }
        _ => return None,
    };
    if tokens.get(function_idx + 1).map(|token| token.kind) != Some(TokenKind::Ident) {
        return None;
    }

    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;
    let mut saw_lparen = false;
    let mut has_top_level_param_comma = false;
    let mut end_idx = None;

    for idx in (function_idx + 2)..tokens.len() {
        if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 && angle_depth == 0 {
            match tokens[idx].kind {
                TokenKind::LBrace | TokenKind::Semi => {
                    end_idx = Some(idx);
                    break;
                }
                TokenKind::Eq | TokenKind::FatArrow => return None,
                _ => {}
            }
        }
        if paren_depth == 1
            && bracket_depth == 0
            && brace_depth == 0
            && angle_depth == 0
            && tokens[idx].kind == TokenKind::Comma
        {
            has_top_level_param_comma = true;
        }

        match tokens[idx].kind {
            TokenKind::LParen => {
                paren_depth += 1;
                saw_lparen = true;
            }
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles.get(idx).copied().unwrap_or(AngleRole::None) {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    let end_idx = end_idx?;
    if !saw_lparen || has_top_level_param_comma || end_idx <= function_idx + 2 {
        return None;
    }
    Some(FunctionDeclHeaderWrap { function_idx })
}

fn find_function_body_wrap(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
) -> Option<FunctionBodyWrap> {
    let function_idx = match tokens.first().map(|token| token.kind) {
        Some(TokenKind::Kw(Keyword::Function)) => 0,
        Some(TokenKind::Kw(Keyword::Pub))
            if tokens.get(1).map(|token| token.kind) == Some(TokenKind::Kw(Keyword::Function)) =>
        {
            1
        }
        _ => return None,
    };
    if tokens.get(function_idx + 1).map(|token| token.kind) != Some(TokenKind::Ident) {
        return None;
    }

    let mut lbrace_idx = None;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in (function_idx + 2)..tokens.len() {
        if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 && angle_depth == 0 {
            match tokens[idx].kind {
                TokenKind::LBrace => {
                    lbrace_idx = Some(idx);
                    break;
                }
                TokenKind::Semi | TokenKind::Eq | TokenKind::FatArrow => return None,
                _ => {}
            }
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles.get(idx).copied().unwrap_or(AngleRole::None) {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    let lbrace_idx = lbrace_idx?;
    if lbrace_idx <= function_idx + 2 || tokens[lbrace_idx].span.range().start > MAX_LINE_LENGTH / 2
    {
        return None;
    }
    find_function_body_wrap_from(tokens, angle_roles, lbrace_idx)
}

fn find_function_body_wrap_from(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    open_idx: usize,
) -> Option<FunctionBodyWrap> {
    let close_idx = matching_brace_close(tokens, open_idx)?;
    if open_idx + 1 >= close_idx {
        return None;
    }
    if close_idx + 1 != tokens.len() {
        return None;
    }
    for body_idx in (open_idx + 1)..close_idx {
        match tokens[body_idx].kind {
            TokenKind::Colon
            | TokenKind::Comma
            | TokenKind::DotDot
            | TokenKind::FatArrow
            | TokenKind::LBrace
            | TokenKind::RBrace => return None,
            kind if arrow_closure_body_operator(kind, angle_roles[body_idx]) => return None,
            _ => {}
        }
    }

    Some(FunctionBodyWrap {
        lbrace_idx: open_idx,
    })
}

fn find_anon_function_header_wrap(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
) -> Option<AnonFunctionHeaderWrap> {
    for idx in 0..tokens.len() {
        if tokens[idx].kind != TokenKind::Kw(Keyword::Function) {
            continue;
        }
        if tokens.get(idx + 1).map(|token| token.kind) == Some(TokenKind::Ident) {
            continue;
        }
        if anon_function_header_body_start(tokens, angle_roles, idx).is_some() {
            return Some(AnonFunctionHeaderWrap { function_idx: idx });
        }
    }

    None
}

fn anon_function_header_body_start(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    function_idx: usize,
) -> Option<usize> {
    let mut idx = function_idx + 1;
    if angle_roles.get(idx).copied() == Some(AngleRole::GenericOpen) {
        idx = matching_generic_close(tokens, angle_roles, idx)? + 1;
    }
    if tokens.get(idx).map(|token| token.kind) != Some(TokenKind::LParen) {
        return None;
    }
    idx = matching_paren_close(tokens, idx)? + 1;
    if tokens.get(idx).map(|token| token.kind) == Some(TokenKind::Colon) {
        idx += 1;
        idx = skip_anon_function_return_type(tokens, angle_roles, idx)?;
    }
    if tokens.get(idx).map(|token| token.kind) == Some(TokenKind::Kw(Keyword::Async)) {
        idx += 1;
    }
    (tokens.get(idx).map(|token| token.kind) == Some(TokenKind::LBrace)).then_some(idx)
}

fn skip_anon_function_return_type(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    start_idx: usize,
) -> Option<usize> {
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in start_idx..tokens.len() {
        if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 && angle_depth == 0 {
            match tokens[idx].kind {
                TokenKind::Kw(Keyword::Async) | TokenKind::LBrace => return Some(idx),
                TokenKind::Eq | TokenKind::Semi | TokenKind::FatArrow | TokenKind::RBrace => {
                    return None;
                }
                _ => {}
            }
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    None
}

fn find_module_decl_header_wrap(tokens: &[&Token]) -> Option<ModuleDeclHeaderWrap> {
    let mod_idx = match tokens.first().map(|token| token.kind) {
        Some(TokenKind::Kw(Keyword::Mod)) => 0,
        Some(TokenKind::Kw(Keyword::Pub))
            if tokens.get(1).map(|token| token.kind) == Some(TokenKind::Kw(Keyword::Mod)) =>
        {
            1
        }
        _ => return None,
    };
    if tokens.get(mod_idx + 1).map(|token| token.kind) != Some(TokenKind::Ident) {
        return None;
    }

    let mut brace_depth = 0usize;
    let mut saw_decl_end = false;
    for idx in (mod_idx + 2)..tokens.len() {
        match tokens[idx].kind {
            TokenKind::Semi if brace_depth == 0 => {
                saw_decl_end = idx + 1 == tokens.len();
                break;
            }
            TokenKind::LBrace => {
                saw_decl_end = true;
                brace_depth += 1;
            }
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }
    }
    if !saw_decl_end {
        return None;
    }

    Some(ModuleDeclHeaderWrap { mod_idx })
}

fn find_module_body_wrap(tokens: &[&Token], angle_roles: &[AngleRole]) -> Option<ModuleBodyWrap> {
    let mod_idx = match tokens.first().map(|token| token.kind) {
        Some(TokenKind::Kw(Keyword::Mod)) => 0,
        Some(TokenKind::Kw(Keyword::Pub))
            if tokens.get(1).map(|token| token.kind) == Some(TokenKind::Kw(Keyword::Mod)) =>
        {
            1
        }
        _ => return None,
    };
    if tokens.get(mod_idx + 1).map(|token| token.kind) != Some(TokenKind::Ident) {
        return None;
    }

    let mut lbrace_idx = None;
    for idx in (mod_idx + 2)..tokens.len() {
        match tokens[idx].kind {
            TokenKind::LBrace => {
                lbrace_idx = Some(idx);
                break;
            }
            TokenKind::Semi => return None,
            _ => {}
        }
    }

    let lbrace_idx = lbrace_idx?;
    if lbrace_idx <= mod_idx + 1 || tokens[lbrace_idx].span.range().start > MAX_LINE_LENGTH / 2 {
        return None;
    }
    find_module_body_wrap_from(tokens, angle_roles, lbrace_idx)
}

fn find_module_body_wrap_from(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    open_idx: usize,
) -> Option<ModuleBodyWrap> {
    let close_idx = matching_brace_close(tokens, open_idx)?;
    if open_idx + 1 >= close_idx {
        return None;
    }
    if close_idx + 1 != tokens.len() {
        return None;
    }
    for body_idx in (open_idx + 1)..close_idx {
        match tokens[body_idx].kind {
            TokenKind::Colon
            | TokenKind::Comma
            | TokenKind::DotDot
            | TokenKind::FatArrow
            | TokenKind::LBrace
            | TokenKind::RBrace => return None,
            kind if arrow_closure_body_operator(kind, angle_roles[body_idx]) => return None,
            _ => {}
        }
    }

    Some(ModuleBodyWrap {
        lbrace_idx: open_idx,
    })
}

fn find_extern_type_wrap(tokens: &[&Token]) -> Option<ExternTypeWrap> {
    let extern_idx = match tokens.first().map(|token| token.kind) {
        Some(TokenKind::Kw(Keyword::Extern)) => 0,
        Some(TokenKind::Kw(Keyword::Pub))
            if tokens.get(1).map(|token| token.kind) == Some(TokenKind::Kw(Keyword::Extern)) =>
        {
            1
        }
        _ => return None,
    };
    let type_idx = extern_idx + 1;
    if tokens.get(type_idx).map(|token| token.kind) != Some(TokenKind::Kw(Keyword::Type)) {
        return None;
    }
    if tokens.get(type_idx + 1).map(|token| token.kind) != Some(TokenKind::Ident) {
        return None;
    }
    let semi_idx = type_idx + 2;
    if tokens.get(semi_idx).map(|token| token.kind) != Some(TokenKind::Semi) {
        return None;
    }
    if tokens.len() != semi_idx + 1 {
        return None;
    }

    Some(ExternTypeWrap { type_idx, semi_idx })
}

fn find_extern_var_wrap(tokens: &[&Token], angle_roles: &[AngleRole]) -> Option<ExternVarWrap> {
    let extern_idx = match tokens.first().map(|token| token.kind) {
        Some(TokenKind::Kw(Keyword::Extern)) => 0,
        Some(TokenKind::Kw(Keyword::Pub))
            if tokens.get(1).map(|token| token.kind) == Some(TokenKind::Kw(Keyword::Extern)) =>
        {
            1
        }
        _ => return None,
    };
    let var_idx = extern_idx + 1;
    if tokens.get(var_idx).map(|token| token.kind) != Some(TokenKind::Kw(Keyword::Var)) {
        return None;
    }
    if tokens.get(var_idx + 1).map(|token| token.kind) != Some(TokenKind::Ident) {
        return None;
    }
    if tokens.get(var_idx + 2).map(|token| token.kind) != Some(TokenKind::Colon) {
        return None;
    }

    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;
    let mut semi_idx = None;
    for idx in (var_idx + 3)..tokens.len() {
        match tokens[idx].kind {
            TokenKind::Semi
                if paren_depth == 0
                    && bracket_depth == 0
                    && brace_depth == 0
                    && angle_depth == 0 =>
            {
                semi_idx = Some(idx);
                break;
            }
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }
        match angle_roles.get(idx).copied().unwrap_or(AngleRole::None) {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }
    let semi_idx = semi_idx?;
    if semi_idx == var_idx + 3 || tokens.len() != semi_idx + 1 {
        return None;
    }

    Some(ExternVarWrap { var_idx, semi_idx })
}

fn find_extern_function_wrap(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
) -> Option<ExternFunctionWrap> {
    let extern_idx = match tokens.first().map(|token| token.kind) {
        Some(TokenKind::Kw(Keyword::Extern)) => 0,
        Some(TokenKind::Kw(Keyword::Pub))
            if tokens.get(1).map(|token| token.kind) == Some(TokenKind::Kw(Keyword::Extern)) =>
        {
            1
        }
        _ => return None,
    };
    let function_idx = extern_idx + 1;
    if tokens.get(function_idx).map(|token| token.kind) != Some(TokenKind::Kw(Keyword::Function)) {
        return None;
    }
    if tokens.get(function_idx + 1).map(|token| token.kind) != Some(TokenKind::Ident) {
        return None;
    }

    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;
    let mut saw_lparen = false;
    let mut semi_idx = None;
    for idx in (function_idx + 2)..tokens.len() {
        match tokens[idx].kind {
            TokenKind::Semi
                if paren_depth == 0
                    && bracket_depth == 0
                    && brace_depth == 0
                    && angle_depth == 0 =>
            {
                semi_idx = Some(idx);
                break;
            }
            TokenKind::LParen => {
                paren_depth += 1;
                saw_lparen = true;
            }
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }
        match angle_roles.get(idx).copied().unwrap_or(AngleRole::None) {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }
    let semi_idx = semi_idx?;
    if !saw_lparen || tokens.len() != semi_idx + 1 {
        return None;
    }

    Some(ExternFunctionWrap {
        function_idx,
        semi_idx,
    })
}

fn find_import_path_wrap(tokens: &[&Token]) -> Option<ImportPathWrap> {
    let import_idx = match tokens.first().map(|token| token.kind) {
        Some(TokenKind::Kw(Keyword::Import)) => 0,
        Some(TokenKind::Kw(Keyword::Pub))
            if tokens.get(1).map(|token| token.kind) == Some(TokenKind::Kw(Keyword::Import)) =>
        {
            1
        }
        _ => return None,
    };

    if tokens.get(import_idx + 1).map(|token| token.kind) != Some(TokenKind::StrStart) {
        return None;
    }
    if !tokens
        .iter()
        .any(|token| matches!(token.kind, TokenKind::Semi))
    {
        return None;
    }

    Some(ImportPathWrap { import_idx })
}

fn find_named_import_path_wrap(
    line: &str,
    tokens: &[&Token],
    angle_roles: &[AngleRole],
) -> Option<NamedImportPathWrap> {
    let import_idx = match tokens.first().map(|token| token.kind) {
        Some(TokenKind::Kw(Keyword::Import)) => 0,
        Some(TokenKind::Kw(Keyword::Pub))
            if tokens.get(1).map(|token| token.kind) == Some(TokenKind::Kw(Keyword::Import)) =>
        {
            1
        }
        _ => return None,
    };

    let lbrace_idx = import_idx + 1;
    if tokens.get(lbrace_idx).map(|token| token.kind) != Some(TokenKind::LBrace) {
        return None;
    }
    let rbrace_idx = matching_brace_close(tokens, lbrace_idx)?;
    if !top_level_commas_in_delimited(tokens, angle_roles, lbrace_idx, rbrace_idx).is_empty() {
        return None;
    }

    let from_idx = rbrace_idx + 1;
    if tokens.get(from_idx).map(|token| token.kind) != Some(TokenKind::Ident)
        || !token_text_eq(line, tokens[from_idx], "from")
    {
        return None;
    }
    if tokens.get(from_idx + 1).map(|token| token.kind) != Some(TokenKind::StrStart) {
        return None;
    }
    if !tokens
        .iter()
        .skip(from_idx + 2)
        .any(|token| token.kind == TokenKind::Semi)
    {
        return None;
    }

    Some(NamedImportPathWrap { from_idx })
}

fn find_attribute_arg_list_wrap(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
) -> Option<AttributeArgListWrap> {
    if tokens.first().map(|token| token.kind) != Some(TokenKind::At)
        || tokens.get(1).map(|token| token.kind) != Some(TokenKind::Ident)
        || tokens.get(2).map(|token| token.kind) != Some(TokenKind::LParen)
    {
        return None;
    }
    let lparen_idx = 2;
    let rparen_idx = matching_paren_close(tokens, lparen_idx)?;
    if rparen_idx + 1 != tokens.len() {
        return None;
    }
    if rparen_idx <= lparen_idx + 1 {
        return None;
    }
    if !top_level_commas_in_delimited(tokens, angle_roles, lparen_idx, rparen_idx).is_empty() {
        return None;
    }

    Some(AttributeArgListWrap {
        lparen_idx,
        rparen_idx,
    })
}

fn token_text_eq(line: &str, token: &Token, expected: &str) -> bool {
    let range = token.span.range();
    line.get(range).is_some_and(|text| text == expected)
}

fn find_arrow_closure_return_union_wrap(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
) -> Option<ArrowClosureReturnUnionWrap> {
    (0..tokens.len())
        .find_map(|idx| find_arrow_closure_return_union_wrap_from(tokens, angle_roles, idx))
}

fn find_arrow_closure_return_union_wrap_from(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    open_idx: usize,
) -> Option<ArrowClosureReturnUnionWrap> {
    if tokens.get(open_idx).map(|token| token.kind) != Some(TokenKind::LParen) {
        return None;
    }

    let close_idx = matching_paren_close(tokens, open_idx)?;
    if tokens.get(close_idx + 1).map(|token| token.kind) != Some(TokenKind::Colon) {
        return None;
    }
    let colon_idx = close_idx + 1;

    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;
    let mut async_idx = None;
    let mut fat_arrow_idx = None;

    for idx in colon_idx + 1..tokens.len() {
        if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 && angle_depth == 0 {
            match tokens[idx].kind {
                TokenKind::Kw(Keyword::Async) => {
                    async_idx = Some(idx);
                    continue;
                }
                TokenKind::FatArrow => {
                    fat_arrow_idx = Some(idx);
                    break;
                }
                TokenKind::Semi | TokenKind::LBrace | TokenKind::RBrace | TokenKind::Eq => {
                    return None;
                }
                _ => {}
            }
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    let fat_arrow_idx = fat_arrow_idx?;
    let end_idx = async_idx.unwrap_or(fat_arrow_idx);
    if colon_idx >= end_idx {
        return None;
    }
    let pipe_indices = top_level_pipes_in_range(tokens, angle_roles, colon_idx, end_idx);
    if pipe_indices.is_empty() {
        return None;
    }
    Some(ArrowClosureReturnUnionWrap {
        colon_idx,
        end_idx,
        fat_arrow_idx,
        pipe_indices,
    })
}

fn find_cast_chain_wrap(tokens: &[&Token], angle_roles: &[AngleRole]) -> Option<CastChainWrap> {
    if has_top_level_cast_chain_breaker(tokens, angle_roles) {
        return None;
    }
    let operator_indices = top_level_cast_chain_operators(tokens, angle_roles);
    (operator_indices.len() >= 2).then_some(CastChainWrap { operator_indices })
}

fn top_level_cast_chain_operators(tokens: &[&Token], angle_roles: &[AngleRole]) -> Vec<usize> {
    let mut operators = Vec::new();
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in 0..tokens.len() {
        if paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && angle_depth == 0
            && matches!(
                tokens[idx].kind,
                TokenKind::Kw(Keyword::As) | TokenKind::Kw(Keyword::Is)
            )
            && plausible_cast_chain_operator(tokens, idx)
        {
            operators.push(idx);
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    operators
}

fn has_top_level_cast_chain_breaker(tokens: &[&Token], angle_roles: &[AngleRole]) -> bool {
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in 0..tokens.len() {
        let top_level =
            paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 && angle_depth == 0;
        if top_level && angle_roles[idx] == AngleRole::None {
            let kind = tokens[idx].kind;
            let definite_binary_breaker = matches!(
                kind,
                TokenKind::AmpAmp
                    | TokenKind::PipePipe
                    | TokenKind::Pipe
                    | TokenKind::Caret
                    | TokenKind::EqEq
                    | TokenKind::BangEq
                    | TokenKind::Lt
                    | TokenKind::LtEq
                    | TokenKind::Gt
                    | TokenKind::GtEq
                    | TokenKind::Shl
                    | TokenKind::Shr
                    | TokenKind::Slash
                    | TokenKind::Percent
                    | TokenKind::FatArrow
            );
            let context_binary_breaker =
                matches!(
                    kind,
                    TokenKind::Plus | TokenKind::Minus | TokenKind::Star | TokenKind::Amp
                ) && is_spaced_operator(kind, angle_roles[idx], tokens, idx);
            if definite_binary_breaker || context_binary_breaker {
                return true;
            }
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    false
}

fn plausible_cast_chain_operator(tokens: &[&Token], idx: usize) -> bool {
    idx > 0
        && idx + 1 < tokens.len()
        && is_cast_operand_end(tokens[idx - 1].kind)
        && is_cast_type_start(tokens[idx + 1].kind)
}

fn is_cast_operand_end(kind: TokenKind) -> bool {
    is_value_end(kind)
        || matches!(
            kind,
            TokenKind::Question | TokenKind::Gt | TokenKind::Shr | TokenKind::Kw(Keyword::Null)
        )
}

fn is_cast_type_start(kind: TokenKind) -> bool {
    is_typeish_start(kind) || matches!(kind, TokenKind::Kw(Keyword::Extern | Keyword::Null))
}

fn find_method_chain_wrap(tokens: &[&Token], angle_roles: &[AngleRole]) -> Option<MethodChainWrap> {
    let dot_indices = top_level_method_chain_dots(tokens, angle_roles);
    (dot_indices.len() >= 2).then_some(MethodChainWrap { dot_indices })
}

fn top_level_method_chain_dots(tokens: &[&Token], angle_roles: &[AngleRole]) -> Vec<usize> {
    let mut dots = Vec::new();
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;
    let mut in_chain = false;

    for idx in 0..tokens.len() {
        let top_level =
            paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 && angle_depth == 0;

        if top_level {
            if tokens[idx].kind == TokenKind::Dot && plausible_method_chain_dot(tokens, idx) {
                dots.push(idx);
                in_chain = true;
            } else if in_chain
                && angle_roles[idx] == AngleRole::None
                && is_method_chain_breaker(tokens[idx].kind)
            {
                return Vec::new();
            }
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    dots
}

fn plausible_method_chain_dot(tokens: &[&Token], idx: usize) -> bool {
    idx > 0
        && tokens.get(idx + 1).is_some_and(|token| {
            matches!(
                token.kind,
                TokenKind::Ident | TokenKind::Int { .. } | TokenKind::Kw(_) | TokenKind::Underscore
            )
        })
}

fn is_method_chain_breaker(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Comma
            | TokenKind::Colon
            | TokenKind::Eq
            | TokenKind::FatArrow
            | TokenKind::Plus
            | TokenKind::Minus
            | TokenKind::Star
            | TokenKind::Slash
            | TokenKind::Percent
            | TokenKind::Caret
            | TokenKind::Amp
            | TokenKind::AmpAmp
            | TokenKind::Pipe
            | TokenKind::PipePipe
            | TokenKind::EqEq
            | TokenKind::BangEq
            | TokenKind::Lt
            | TokenKind::LtEq
            | TokenKind::Gt
            | TokenKind::GtEq
            | TokenKind::Shl
            | TokenKind::Shr
            | TokenKind::DotDot
    )
}

fn find_logical_chain_wrap(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
) -> Option<LogicalChainWrap> {
    let operator_indices = top_level_logical_chain_operators(tokens, angle_roles);
    (operator_indices.len() >= 2).then_some(LogicalChainWrap { operator_indices })
}

fn top_level_logical_chain_operators(tokens: &[&Token], angle_roles: &[AngleRole]) -> Vec<usize> {
    let mut operators = Vec::new();
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in 0..tokens.len() {
        if paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && angle_depth == 0
            && matches!(tokens[idx].kind, TokenKind::AmpAmp | TokenKind::PipePipe)
        {
            operators.push(idx);
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    operators
}

fn find_comparison_expr_wrap(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
) -> Option<ComparisonExprWrap> {
    if has_top_level_comparison_expr_breaker(tokens, angle_roles) {
        return None;
    }
    let operator_indices = top_level_comparison_expr_operators(tokens, angle_roles);
    (operator_indices.len() == 1).then(|| ComparisonExprWrap {
        operator_idx: operator_indices[0],
    })
}

fn top_level_comparison_expr_operators(tokens: &[&Token], angle_roles: &[AngleRole]) -> Vec<usize> {
    let mut operators = Vec::new();
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in 0..tokens.len() {
        if paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && angle_depth == 0
            && matches!(
                tokens[idx].kind,
                TokenKind::EqEq
                    | TokenKind::BangEq
                    | TokenKind::Lt
                    | TokenKind::LtEq
                    | TokenKind::Gt
                    | TokenKind::GtEq
            )
            && angle_roles[idx] == AngleRole::None
            && plausible_comparison_operator(tokens, idx)
        {
            operators.push(idx);
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    operators
}

fn has_top_level_comparison_expr_breaker(tokens: &[&Token], angle_roles: &[AngleRole]) -> bool {
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in 0..tokens.len() {
        if paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && angle_depth == 0
            && matches!(
                tokens[idx].kind,
                TokenKind::AmpAmp | TokenKind::PipePipe | TokenKind::FatArrow
            )
            && angle_roles[idx] == AngleRole::None
        {
            return true;
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    false
}

fn plausible_comparison_operator(tokens: &[&Token], idx: usize) -> bool {
    idx > 0
        && idx + 1 < tokens.len()
        && is_value_end(tokens[idx - 1].kind)
        && is_comparison_operand_start(tokens[idx + 1].kind)
}

fn is_comparison_operand_start(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Ident
            | TokenKind::Underscore
            | TokenKind::Int { .. }
            | TokenKind::Float { .. }
            | TokenKind::Char
            | TokenKind::StrStart
            | TokenKind::LParen
            | TokenKind::LBracket
            | TokenKind::LBrace
            | TokenKind::Bang
            | TokenKind::Tilde
            | TokenKind::Plus
            | TokenKind::Minus
            | TokenKind::Star
            | TokenKind::Amp
            | TokenKind::Kw(Keyword::True)
            | TokenKind::Kw(Keyword::False)
            | TokenKind::Kw(Keyword::Null)
            | TokenKind::Kw(Keyword::SelfLower)
            | TokenKind::Kw(Keyword::If)
            | TokenKind::Kw(Keyword::Match)
            | TokenKind::Kw(Keyword::Spawn)
            | TokenKind::Kw(Keyword::Async)
    )
}

fn find_additive_chain_wrap(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
) -> Option<AdditiveChainWrap> {
    if has_top_level_additive_chain_breaker(tokens, angle_roles) {
        return None;
    }
    let operator_indices = top_level_additive_chain_operators(tokens, angle_roles);
    (operator_indices.len() >= 2).then_some(AdditiveChainWrap { operator_indices })
}

fn top_level_additive_chain_operators(tokens: &[&Token], angle_roles: &[AngleRole]) -> Vec<usize> {
    let mut operators = Vec::new();
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in 0..tokens.len() {
        if paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && angle_depth == 0
            && matches!(tokens[idx].kind, TokenKind::Plus | TokenKind::Minus)
            && plausible_additive_operator(tokens, idx)
        {
            operators.push(idx);
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    operators
}

fn plausible_additive_operator(tokens: &[&Token], idx: usize) -> bool {
    idx > 0
        && idx + 1 < tokens.len()
        && is_additive_operand_end(tokens[idx - 1].kind)
        && is_additive_operand_start(tokens[idx + 1].kind)
}

fn has_top_level_additive_chain_breaker(tokens: &[&Token], angle_roles: &[AngleRole]) -> bool {
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in 0..tokens.len() {
        if paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && angle_depth == 0
            && matches!(
                tokens[idx].kind,
                TokenKind::AmpAmp
                    | TokenKind::PipePipe
                    | TokenKind::Pipe
                    | TokenKind::Caret
                    | TokenKind::Amp
                    | TokenKind::EqEq
                    | TokenKind::BangEq
                    | TokenKind::Lt
                    | TokenKind::LtEq
                    | TokenKind::Gt
                    | TokenKind::GtEq
                    | TokenKind::Shl
                    | TokenKind::Shr
                    | TokenKind::FatArrow
            )
            && angle_roles[idx] == AngleRole::None
        {
            return true;
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    false
}

fn is_additive_operand_end(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Ident
            | TokenKind::Underscore
            | TokenKind::Int { .. }
            | TokenKind::Float { .. }
            | TokenKind::Char
            | TokenKind::StrEnd
            | TokenKind::RParen
            | TokenKind::RBracket
            | TokenKind::RBrace
            | TokenKind::Question
            | TokenKind::Kw(Keyword::True)
            | TokenKind::Kw(Keyword::False)
            | TokenKind::Kw(Keyword::Null)
            | TokenKind::Kw(Keyword::SelfLower)
    )
}

fn is_additive_operand_start(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Ident
            | TokenKind::Underscore
            | TokenKind::Int { .. }
            | TokenKind::Float { .. }
            | TokenKind::Char
            | TokenKind::StrStart
            | TokenKind::LParen
            | TokenKind::LBracket
            | TokenKind::LBrace
            | TokenKind::Bang
            | TokenKind::Tilde
            | TokenKind::Star
            | TokenKind::Amp
            | TokenKind::Kw(Keyword::True)
            | TokenKind::Kw(Keyword::False)
            | TokenKind::Kw(Keyword::Null)
            | TokenKind::Kw(Keyword::SelfLower)
            | TokenKind::Kw(Keyword::If)
            | TokenKind::Kw(Keyword::Match)
            | TokenKind::Kw(Keyword::Spawn)
            | TokenKind::Kw(Keyword::Async)
    )
}

fn find_multiplicative_chain_wrap(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
) -> Option<MultiplicativeChainWrap> {
    if has_top_level_multiplicative_chain_breaker(tokens, angle_roles) {
        return None;
    }
    let operator_indices = top_level_multiplicative_chain_operators(tokens, angle_roles);
    (operator_indices.len() >= 2).then_some(MultiplicativeChainWrap { operator_indices })
}

fn top_level_multiplicative_chain_operators(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
) -> Vec<usize> {
    let mut operators = Vec::new();
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in 0..tokens.len() {
        if paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && angle_depth == 0
            && matches!(
                tokens[idx].kind,
                TokenKind::Star | TokenKind::Slash | TokenKind::Percent
            )
            && plausible_multiplicative_operator(tokens, idx)
        {
            operators.push(idx);
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    operators
}

fn has_top_level_multiplicative_chain_breaker(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
) -> bool {
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in 0..tokens.len() {
        if paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && angle_depth == 0
            && match tokens[idx].kind {
                TokenKind::Plus | TokenKind::Minus => plausible_additive_operator(tokens, idx),
                TokenKind::AmpAmp
                | TokenKind::PipePipe
                | TokenKind::Pipe
                | TokenKind::Caret
                | TokenKind::Amp
                | TokenKind::EqEq
                | TokenKind::BangEq
                | TokenKind::Lt
                | TokenKind::LtEq
                | TokenKind::Gt
                | TokenKind::GtEq
                | TokenKind::Shl
                | TokenKind::Shr
                | TokenKind::FatArrow => angle_roles[idx] == AngleRole::None,
                _ => false,
            }
        {
            return true;
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    false
}

fn plausible_multiplicative_operator(tokens: &[&Token], idx: usize) -> bool {
    idx > 0
        && idx + 1 < tokens.len()
        && is_additive_operand_end(tokens[idx - 1].kind)
        && is_multiplicative_operand_start(tokens[idx + 1].kind)
}

fn is_multiplicative_operand_start(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Ident
            | TokenKind::Underscore
            | TokenKind::Int { .. }
            | TokenKind::Float { .. }
            | TokenKind::Char
            | TokenKind::StrStart
            | TokenKind::LParen
            | TokenKind::LBracket
            | TokenKind::LBrace
            | TokenKind::Bang
            | TokenKind::Tilde
            | TokenKind::Plus
            | TokenKind::Minus
            | TokenKind::Star
            | TokenKind::Amp
            | TokenKind::Kw(Keyword::True)
            | TokenKind::Kw(Keyword::False)
            | TokenKind::Kw(Keyword::Null)
            | TokenKind::Kw(Keyword::SelfLower)
            | TokenKind::Kw(Keyword::If)
            | TokenKind::Kw(Keyword::Match)
            | TokenKind::Kw(Keyword::Spawn)
            | TokenKind::Kw(Keyword::Async)
    )
}

fn find_shift_chain_wrap(tokens: &[&Token], angle_roles: &[AngleRole]) -> Option<ShiftChainWrap> {
    if has_top_level_shift_chain_breaker(tokens, angle_roles) {
        return None;
    }
    let operator_indices = top_level_shift_chain_operators(tokens, angle_roles);
    (operator_indices.len() >= 2).then_some(ShiftChainWrap { operator_indices })
}

fn top_level_shift_chain_operators(tokens: &[&Token], angle_roles: &[AngleRole]) -> Vec<usize> {
    let mut operators = Vec::new();
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in 0..tokens.len() {
        if paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && angle_depth == 0
            && matches!(tokens[idx].kind, TokenKind::Shl | TokenKind::Shr)
            && angle_roles[idx] == AngleRole::None
            && plausible_shift_operator(tokens, idx)
        {
            operators.push(idx);
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    operators
}

fn has_top_level_shift_chain_breaker(tokens: &[&Token], angle_roles: &[AngleRole]) -> bool {
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in 0..tokens.len() {
        if paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && angle_depth == 0
            && matches!(
                tokens[idx].kind,
                TokenKind::AmpAmp
                    | TokenKind::PipePipe
                    | TokenKind::Pipe
                    | TokenKind::Caret
                    | TokenKind::Amp
                    | TokenKind::EqEq
                    | TokenKind::BangEq
                    | TokenKind::Lt
                    | TokenKind::LtEq
                    | TokenKind::Gt
                    | TokenKind::GtEq
                    | TokenKind::FatArrow
            )
            && angle_roles[idx] == AngleRole::None
        {
            return true;
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    false
}

fn plausible_shift_operator(tokens: &[&Token], idx: usize) -> bool {
    idx > 0
        && idx + 1 < tokens.len()
        && is_additive_operand_end(tokens[idx - 1].kind)
        && is_shift_operand_start(tokens[idx + 1].kind)
}

fn is_shift_operand_start(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Ident
            | TokenKind::Underscore
            | TokenKind::Int { .. }
            | TokenKind::Float { .. }
            | TokenKind::Char
            | TokenKind::StrStart
            | TokenKind::LParen
            | TokenKind::LBracket
            | TokenKind::LBrace
            | TokenKind::Bang
            | TokenKind::Tilde
            | TokenKind::Plus
            | TokenKind::Minus
            | TokenKind::Star
            | TokenKind::Amp
            | TokenKind::Kw(Keyword::True)
            | TokenKind::Kw(Keyword::False)
            | TokenKind::Kw(Keyword::Null)
            | TokenKind::Kw(Keyword::SelfLower)
            | TokenKind::Kw(Keyword::If)
            | TokenKind::Kw(Keyword::Match)
            | TokenKind::Kw(Keyword::Spawn)
            | TokenKind::Kw(Keyword::Async)
    )
}

fn find_bitwise_and_chain_wrap(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
) -> Option<BitwiseAndChainWrap> {
    if has_top_level_bitwise_and_chain_breaker(tokens, angle_roles) {
        return None;
    }
    let operator_indices = top_level_bitwise_and_chain_operators(tokens, angle_roles);
    (operator_indices.len() >= 2).then_some(BitwiseAndChainWrap { operator_indices })
}

fn top_level_bitwise_and_chain_operators(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
) -> Vec<usize> {
    let mut operators = Vec::new();
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in 0..tokens.len() {
        if paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && angle_depth == 0
            && tokens[idx].kind == TokenKind::Amp
            && plausible_bitwise_and_operator(tokens, idx)
        {
            operators.push(idx);
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    operators
}

fn has_top_level_bitwise_and_chain_breaker(tokens: &[&Token], angle_roles: &[AngleRole]) -> bool {
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in 0..tokens.len() {
        if paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && angle_depth == 0
            && matches!(
                tokens[idx].kind,
                TokenKind::AmpAmp
                    | TokenKind::PipePipe
                    | TokenKind::Pipe
                    | TokenKind::Caret
                    | TokenKind::EqEq
                    | TokenKind::BangEq
                    | TokenKind::Lt
                    | TokenKind::LtEq
                    | TokenKind::Gt
                    | TokenKind::GtEq
                    | TokenKind::FatArrow
            )
            && angle_roles[idx] == AngleRole::None
        {
            return true;
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    false
}

fn plausible_bitwise_and_operator(tokens: &[&Token], idx: usize) -> bool {
    idx > 0
        && idx + 1 < tokens.len()
        && is_additive_operand_end(tokens[idx - 1].kind)
        && is_bitwise_and_operand_start(tokens[idx + 1].kind)
}

fn is_bitwise_and_operand_start(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Ident
            | TokenKind::Underscore
            | TokenKind::Int { .. }
            | TokenKind::Float { .. }
            | TokenKind::Char
            | TokenKind::StrStart
            | TokenKind::LParen
            | TokenKind::LBracket
            | TokenKind::LBrace
            | TokenKind::Bang
            | TokenKind::Tilde
            | TokenKind::Plus
            | TokenKind::Minus
            | TokenKind::Star
            | TokenKind::Amp
            | TokenKind::Kw(Keyword::True)
            | TokenKind::Kw(Keyword::False)
            | TokenKind::Kw(Keyword::Null)
            | TokenKind::Kw(Keyword::SelfLower)
            | TokenKind::Kw(Keyword::If)
            | TokenKind::Kw(Keyword::Match)
            | TokenKind::Kw(Keyword::Spawn)
            | TokenKind::Kw(Keyword::Async)
    )
}

fn find_bitwise_xor_chain_wrap(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
) -> Option<BitwiseXorChainWrap> {
    if has_top_level_bitwise_xor_chain_breaker(tokens, angle_roles) {
        return None;
    }
    let operator_indices = top_level_bitwise_xor_chain_operators(tokens, angle_roles);
    (operator_indices.len() >= 2).then_some(BitwiseXorChainWrap { operator_indices })
}

fn top_level_bitwise_xor_chain_operators(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
) -> Vec<usize> {
    let mut operators = Vec::new();
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in 0..tokens.len() {
        if paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && angle_depth == 0
            && tokens[idx].kind == TokenKind::Caret
            && plausible_bitwise_xor_operator(tokens, idx)
        {
            operators.push(idx);
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    operators
}

fn has_top_level_bitwise_xor_chain_breaker(tokens: &[&Token], angle_roles: &[AngleRole]) -> bool {
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in 0..tokens.len() {
        if paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && angle_depth == 0
            && matches!(
                tokens[idx].kind,
                TokenKind::AmpAmp
                    | TokenKind::PipePipe
                    | TokenKind::Pipe
                    | TokenKind::EqEq
                    | TokenKind::BangEq
                    | TokenKind::Lt
                    | TokenKind::LtEq
                    | TokenKind::Gt
                    | TokenKind::GtEq
                    | TokenKind::FatArrow
            )
            && angle_roles[idx] == AngleRole::None
        {
            return true;
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    false
}

fn plausible_bitwise_xor_operator(tokens: &[&Token], idx: usize) -> bool {
    idx > 0
        && idx + 1 < tokens.len()
        && is_additive_operand_end(tokens[idx - 1].kind)
        && is_bitwise_xor_operand_start(tokens[idx + 1].kind)
}

fn is_bitwise_xor_operand_start(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Ident
            | TokenKind::Underscore
            | TokenKind::Int { .. }
            | TokenKind::Float { .. }
            | TokenKind::Char
            | TokenKind::StrStart
            | TokenKind::LParen
            | TokenKind::LBracket
            | TokenKind::LBrace
            | TokenKind::Bang
            | TokenKind::Tilde
            | TokenKind::Plus
            | TokenKind::Minus
            | TokenKind::Star
            | TokenKind::Amp
            | TokenKind::Kw(Keyword::True)
            | TokenKind::Kw(Keyword::False)
            | TokenKind::Kw(Keyword::Null)
            | TokenKind::Kw(Keyword::SelfLower)
            | TokenKind::Kw(Keyword::If)
            | TokenKind::Kw(Keyword::Match)
            | TokenKind::Kw(Keyword::Spawn)
            | TokenKind::Kw(Keyword::Async)
    )
}

fn find_bitwise_or_chain_wrap(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
) -> Option<BitwiseOrChainWrap> {
    if has_top_level_bitwise_or_chain_breaker(tokens, angle_roles) {
        return None;
    }
    let operator_indices = top_level_bitwise_or_chain_operators(tokens, angle_roles);
    (operator_indices.len() >= 2).then_some(BitwiseOrChainWrap { operator_indices })
}

fn top_level_bitwise_or_chain_operators(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
) -> Vec<usize> {
    let mut operators = Vec::new();
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in 0..tokens.len() {
        if paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && angle_depth == 0
            && tokens[idx].kind == TokenKind::Pipe
            && plausible_bitwise_or_operator(tokens, idx)
        {
            operators.push(idx);
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    operators
}

fn has_top_level_bitwise_or_chain_breaker(tokens: &[&Token], angle_roles: &[AngleRole]) -> bool {
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in 0..tokens.len() {
        if paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && angle_depth == 0
            && matches!(
                tokens[idx].kind,
                TokenKind::AmpAmp
                    | TokenKind::PipePipe
                    | TokenKind::EqEq
                    | TokenKind::BangEq
                    | TokenKind::Lt
                    | TokenKind::LtEq
                    | TokenKind::Gt
                    | TokenKind::GtEq
                    | TokenKind::FatArrow
            )
            && angle_roles[idx] == AngleRole::None
        {
            return true;
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    false
}

fn plausible_bitwise_or_operator(tokens: &[&Token], idx: usize) -> bool {
    idx > 0
        && idx + 1 < tokens.len()
        && is_additive_operand_end(tokens[idx - 1].kind)
        && is_bitwise_or_operand_start(tokens[idx + 1].kind)
}

fn is_bitwise_or_operand_start(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Ident
            | TokenKind::Underscore
            | TokenKind::Int { .. }
            | TokenKind::Float { .. }
            | TokenKind::Char
            | TokenKind::StrStart
            | TokenKind::LParen
            | TokenKind::LBracket
            | TokenKind::LBrace
            | TokenKind::Bang
            | TokenKind::Tilde
            | TokenKind::Plus
            | TokenKind::Minus
            | TokenKind::Star
            | TokenKind::Amp
            | TokenKind::Kw(Keyword::True)
            | TokenKind::Kw(Keyword::False)
            | TokenKind::Kw(Keyword::Null)
            | TokenKind::Kw(Keyword::SelfLower)
            | TokenKind::Kw(Keyword::If)
            | TokenKind::Kw(Keyword::Match)
            | TokenKind::Kw(Keyword::Spawn)
            | TokenKind::Kw(Keyword::Async)
    )
}

fn find_delimited_wrap(tokens: &[&Token], angle_roles: &[AngleRole]) -> Option<DelimitedWrap> {
    let mut stack = Vec::new();
    let mut best = None;
    for (idx, token) in tokens.iter().enumerate() {
        match token.kind {
            TokenKind::LParen => stack.push((idx, WrapDelimiter::Paren)),
            TokenKind::LBracket => stack.push((idx, WrapDelimiter::Bracket)),
            TokenKind::Lt if angle_roles[idx] == AngleRole::GenericOpen => {
                stack.push((idx, WrapDelimiter::Angle));
            }
            TokenKind::LBrace => {
                if let Some(kind) = brace_wrap_open_kind(tokens, angle_roles, idx) {
                    stack.push((idx, WrapDelimiter::Brace(kind)));
                }
            }
            TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace => {
                consider_delimiter_close(
                    tokens,
                    angle_roles,
                    &mut stack,
                    idx,
                    token.span.range().start,
                    token.span.range().start,
                    true,
                    token.kind,
                    &mut best,
                );
            }
            TokenKind::Gt if angle_roles[idx] == AngleRole::GenericClose => {
                consider_delimiter_close(
                    tokens,
                    angle_roles,
                    &mut stack,
                    idx,
                    token.span.range().start,
                    token.span.range().start,
                    true,
                    TokenKind::Gt,
                    &mut best,
                );
            }
            TokenKind::Shr if angle_roles[idx] == AngleRole::GenericCloseClose => {
                let close_start = token.span.range().start;
                discard_delimiter_close(&mut stack, TokenKind::Gt);
                consider_delimiter_close(
                    tokens,
                    angle_roles,
                    &mut stack,
                    idx,
                    close_start + 1,
                    token.span.range().end,
                    false,
                    TokenKind::Gt,
                    &mut best,
                );
            }
            _ => {}
        }
    }
    best
}

fn consider_delimiter_close(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    stack: &mut Vec<(usize, WrapDelimiter)>,
    close_idx: usize,
    close_start: usize,
    suffix_start: usize,
    close_on_own_line: bool,
    close_kind: TokenKind,
    best: &mut Option<DelimitedWrap>,
) {
    let Some((open_idx, open_delimiter)) = stack.pop() else {
        return;
    };
    if !delimiter_matches_close(open_delimiter, close_kind) {
        return;
    }
    let comma_indices = top_level_commas_in_delimited(tokens, angle_roles, open_idx, close_idx);
    if comma_indices.is_empty() {
        return;
    }
    if matches!(
        open_delimiter,
        WrapDelimiter::Brace(BraceWrapKind::MapLiteralCandidate)
    ) && !has_top_level_map_literal_marker(tokens, angle_roles, open_idx, close_idx)
    {
        return;
    }
    if matches!(
        open_delimiter,
        WrapDelimiter::Brace(BraceWrapKind::MatchArms)
    ) && !has_top_level_match_arm_marker(tokens, angle_roles, open_idx, close_idx)
    {
        return;
    }
    let candidate = DelimitedWrap {
        delimiter: open_delimiter,
        open_idx,
        close_idx,
        last_end: if close_on_own_line {
            close_start
        } else {
            suffix_start
        },
        suffix_start,
        close_on_own_line,
        comma_indices,
    };
    let width = tokens[close_idx].span.range().end - tokens[open_idx].span.range().start;
    let best_width = best
        .as_ref()
        .map(|pair| {
            tokens[pair.close_idx].span.range().end - tokens[pair.open_idx].span.range().start
        })
        .unwrap_or(0);
    if width > best_width {
        *best = Some(candidate);
    }
}

fn discard_delimiter_close(stack: &mut Vec<(usize, WrapDelimiter)>, close_kind: TokenKind) {
    let Some((_, open_delimiter)) = stack.pop() else {
        return;
    };
    let _ = delimiter_matches_close(open_delimiter, close_kind);
}

fn delimiter_matches_close(open_delimiter: WrapDelimiter, close_kind: TokenKind) -> bool {
    matches!(
        (open_delimiter, close_kind),
        (WrapDelimiter::Paren, TokenKind::RParen)
            | (WrapDelimiter::Bracket, TokenKind::RBracket)
            | (WrapDelimiter::Brace(_), TokenKind::RBrace)
            | (WrapDelimiter::Angle, TokenKind::Gt)
    )
}

fn brace_wrap_open_kind(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    idx: usize,
) -> Option<BraceWrapKind> {
    if idx == 0 {
        return None;
    }
    if matches!(tokens[idx - 1].kind, TokenKind::Kw(Keyword::Import)) {
        return Some(BraceWrapKind::NamedImport);
    }
    if plausible_match_arm_brace_wrap_open(tokens, idx) {
        return Some(BraceWrapKind::MatchArms);
    }
    if plausible_record_decl_brace_wrap_open(tokens, angle_roles, idx) {
        return Some(BraceWrapKind::RecordDecl);
    }
    if matches!(
        tokens[idx - 1].kind,
        TokenKind::Ident | TokenKind::Kw(Keyword::SelfUpper)
    ) || angle_roles[idx - 1].is_generic_close()
    {
        return Some(BraceWrapKind::TypeLed);
    }
    if plausible_map_literal_brace_wrap_open(tokens, idx) {
        return Some(BraceWrapKind::MapLiteralCandidate);
    }
    None
}

fn plausible_map_literal_brace_wrap_open(tokens: &[&Token], idx: usize) -> bool {
    matches!(
        tokens[idx - 1].kind,
        TokenKind::Eq
            | TokenKind::FatArrow
            | TokenKind::LParen
            | TokenKind::LBracket
            | TokenKind::Comma
            | TokenKind::Colon
            | TokenKind::Kw(Keyword::Return)
    )
}

fn plausible_record_decl_brace_wrap_open(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    idx: usize,
) -> bool {
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for scan_idx in (0..idx).rev() {
        if paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && angle_depth == 0
            && matches!(tokens[scan_idx].kind, TokenKind::Kw(Keyword::Struct))
        {
            return true;
        }

        if paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && angle_depth == 0
            && matches!(
                tokens[scan_idx].kind,
                TokenKind::Eq
                    | TokenKind::Semi
                    | TokenKind::Comma
                    | TokenKind::FatArrow
                    | TokenKind::RBrace
                    | TokenKind::Kw(Keyword::Function)
                    | TokenKind::Kw(Keyword::Interface)
                    | TokenKind::Kw(Keyword::Extend)
                    | TokenKind::Kw(Keyword::If)
                    | TokenKind::Kw(Keyword::For)
                    | TokenKind::Kw(Keyword::While)
                    | TokenKind::Kw(Keyword::Loop)
                    | TokenKind::Kw(Keyword::Match)
            )
        {
            return false;
        }

        match tokens[scan_idx].kind {
            TokenKind::RParen => paren_depth += 1,
            TokenKind::LParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::RBracket => bracket_depth += 1,
            TokenKind::LBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::RBrace => brace_depth += 1,
            TokenKind::LBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[scan_idx] {
            AngleRole::GenericClose => angle_depth += 1,
            AngleRole::GenericCloseClose => angle_depth += 2,
            AngleRole::GenericOpen => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::None => {}
        }
    }

    false
}

fn plausible_match_arm_brace_wrap_open(tokens: &[&Token], idx: usize) -> bool {
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for scan_idx in (0..idx).rev() {
        if paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && angle_depth == 0
            && matches!(tokens[scan_idx].kind, TokenKind::Kw(Keyword::Match))
        {
            return true;
        }

        if paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && angle_depth == 0
            && matches!(
                tokens[scan_idx].kind,
                TokenKind::Semi | TokenKind::Comma | TokenKind::FatArrow
            )
        {
            return false;
        }

        match tokens[scan_idx].kind {
            TokenKind::RParen => paren_depth += 1,
            TokenKind::LParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::RBracket => bracket_depth += 1,
            TokenKind::LBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::RBrace => brace_depth += 1,
            TokenKind::LBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match tokens[scan_idx].kind {
            TokenKind::Gt | TokenKind::Shr => angle_depth += 1,
            TokenKind::Lt => angle_depth = angle_depth.saturating_sub(1),
            _ => {}
        }
    }

    false
}

fn has_top_level_map_literal_marker(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    open_idx: usize,
    close_idx: usize,
) -> bool {
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in open_idx + 1..close_idx {
        if paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && angle_depth == 0
            && matches!(tokens[idx].kind, TokenKind::Colon | TokenKind::DotDot)
        {
            return true;
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    false
}

fn has_top_level_match_arm_marker(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    open_idx: usize,
    close_idx: usize,
) -> bool {
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in open_idx + 1..close_idx {
        if paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && angle_depth == 0
            && tokens[idx].kind == TokenKind::FatArrow
        {
            return true;
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    false
}

fn matching_brace_close(tokens: &[&Token], open_idx: usize) -> Option<usize> {
    if tokens.get(open_idx).map(|token| token.kind) != Some(TokenKind::LBrace) {
        return None;
    }
    let mut depth = 0usize;
    for (idx, token) in tokens.iter().enumerate().skip(open_idx + 1) {
        match token.kind {
            TokenKind::LBrace => depth += 1,
            TokenKind::RBrace if depth == 0 => return Some(idx),
            TokenKind::RBrace => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    None
}

fn matching_paren_close(tokens: &[&Token], open_idx: usize) -> Option<usize> {
    if tokens.get(open_idx).map(|token| token.kind) != Some(TokenKind::LParen) {
        return None;
    }
    let mut depth = 0usize;
    for (idx, token) in tokens.iter().enumerate().skip(open_idx + 1) {
        match token.kind {
            TokenKind::LParen => depth += 1,
            TokenKind::RParen if depth == 0 => return Some(idx),
            TokenKind::RParen => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    None
}

fn top_level_interface_member_ranges(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    open_idx: usize,
    close_idx: usize,
) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut member_start = None;
    let mut member_has_function = false;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in open_idx + 1..close_idx {
        let top_level =
            paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 && angle_depth == 0;

        if top_level && member_start.is_none() {
            member_start = Some(tokens[idx].span.range().start);
        }
        if top_level && matches!(tokens[idx].kind, TokenKind::Kw(Keyword::Function)) {
            member_has_function = true;
        }

        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace if brace_depth > 0 => {
                brace_depth -= 1;
                if brace_depth == 0
                    && paren_depth == 0
                    && bracket_depth == 0
                    && angle_depth == 0
                    && member_has_function
                {
                    if let Some(start) = member_start.take() {
                        ranges.push((start, tokens[idx].span.range().end));
                    }
                    member_has_function = false;
                }
            }
            TokenKind::Semi if top_level && member_has_function => {
                if let Some(start) = member_start.take() {
                    ranges.push((start, tokens[idx].span.range().end));
                }
                member_has_function = false;
            }
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    ranges
}

fn top_level_commas_in_delimited(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    open_idx: usize,
    close_idx: usize,
) -> Vec<usize> {
    let mut commas = Vec::new();
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in open_idx + 1..close_idx {
        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            TokenKind::Comma
                if paren_depth == 0
                    && bracket_depth == 0
                    && brace_depth == 0
                    && angle_depth == 0 =>
            {
                commas.push(idx);
            }
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    commas
}

fn top_level_pipes_in_range(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    start_idx: usize,
    end_idx: usize,
) -> Vec<usize> {
    let mut pipes = Vec::new();
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in start_idx + 1..end_idx {
        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            TokenKind::Pipe
                if paren_depth == 0
                    && bracket_depth == 0
                    && brace_depth == 0
                    && angle_depth == 0 =>
            {
                pipes.push(idx);
            }
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    pipes
}

fn top_level_pluses_in_range(
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    start_idx: usize,
    end_idx: usize,
) -> Vec<usize> {
    let mut pluses = Vec::new();
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;

    for idx in start_idx + 1..end_idx {
        match tokens[idx].kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace => brace_depth += 1,
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            TokenKind::Plus
                if paren_depth == 0
                    && bracket_depth == 0
                    && brace_depth == 0
                    && angle_depth == 0 =>
            {
                pluses.push(idx);
            }
            _ => {}
        }

        match angle_roles[idx] {
            AngleRole::GenericOpen => angle_depth += 1,
            AngleRole::GenericClose => angle_depth = angle_depth.saturating_sub(1),
            AngleRole::GenericCloseClose => angle_depth = angle_depth.saturating_sub(2),
            AngleRole::None => {}
        }
    }

    pluses
}

fn render_generic_bound_edge_range(
    code: &str,
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    start_idx: usize,
    end_idx: usize,
) -> String {
    let mut out = String::new();
    for idx in start_idx..end_idx {
        let curr_text = tok_text(code, tokens[idx]);
        if idx > start_idx {
            let prev = tokens[idx - 1];
            let prev_text = tok_text(code, prev);
            let sep = forced_generic_bound_edge_spacing(prev.kind, tokens[idx].kind)
                .unwrap_or_else(|| {
                    spacing_between(
                        prev,
                        tokens[idx],
                        tokens,
                        angle_roles,
                        idx,
                        prev_text,
                        curr_text,
                    )
                });
            out.push_str(sep);
        }
        out.push_str(curr_text);
    }
    out
}

fn forced_generic_bound_edge_spacing(
    prev_kind: TokenKind,
    curr_kind: TokenKind,
) -> Option<&'static str> {
    if prev_kind == TokenKind::Lt || curr_kind == TokenKind::Lt {
        return Some("");
    }
    if matches!(prev_kind, TokenKind::Gt | TokenKind::Shr) {
        if curr_kind == TokenKind::LBrace {
            return Some(" ");
        }
        return if matches!(
            curr_kind,
            TokenKind::LParen
                | TokenKind::LBracket
                | TokenKind::Dot
                | TokenKind::Question
                | TokenKind::Comma
                | TokenKind::Semi
                | TokenKind::Colon
                | TokenKind::RParen
                | TokenKind::RBracket
                | TokenKind::RBrace
                | TokenKind::Gt
                | TokenKind::Shr
        ) {
            Some("")
        } else {
            Some(" ")
        };
    }
    if matches!(curr_kind, TokenKind::Gt | TokenKind::Shr) {
        return Some("");
    }
    None
}

fn spacing_between<'a>(
    prev: &Token,
    curr: &Token,
    tokens: &[&Token],
    angle_roles: &[AngleRole],
    curr_idx: usize,
    prev_text: &str,
    curr_text: &str,
) -> &'a str {
    let prev_kind = prev.kind;
    let curr_kind = curr.kind;
    let prev_role = angle_roles[curr_idx - 1];
    let curr_role = angle_roles[curr_idx];

    if matches!(
        prev_kind,
        TokenKind::LParen | TokenKind::LBracket | TokenKind::At | TokenKind::Dot
    ) || matches!(
        curr_kind,
        TokenKind::RParen
            | TokenKind::RBracket
            | TokenKind::Comma
            | TokenKind::Semi
            | TokenKind::Colon
            | TokenKind::Dot
            | TokenKind::DotDot
            | TokenKind::Question
    ) {
        return "";
    }

    if matches!(
        prev_kind,
        TokenKind::Comma | TokenKind::Semi | TokenKind::Colon
    ) {
        return " ";
    }

    if matches!(
        curr_kind,
        TokenKind::StrText | TokenKind::DollarIdent | TokenKind::DollarLBrace | TokenKind::StrEnd
    ) || matches!(
        prev_kind,
        TokenKind::StrStart | TokenKind::StrText | TokenKind::DollarIdent | TokenKind::DollarLBrace
    ) {
        return "";
    }

    if prev_kind == TokenKind::Gt
        && curr_kind == TokenKind::Gt
        && prev_role == AngleRole::GenericClose
        && curr_role == AngleRole::GenericClose
    {
        // Two already-separate `>` tokens must remain separate; merging into
        // `>>` would change the parser token stream and fail the safety gate.
        return " ";
    }

    if curr_kind == TokenKind::Kw(Keyword::Async)
        && matches!(prev_kind, TokenKind::RParen | TokenKind::RBrace)
    {
        return " ";
    }

    if curr_role == AngleRole::GenericOpen
        || prev_role == AngleRole::GenericOpen
        || curr_role.is_generic_close()
    {
        return "";
    }

    if curr_kind == TokenKind::Lt
        && curr_idx + 1 == tokens.len()
        && matches!(
            prev_kind,
            TokenKind::Ident | TokenKind::Kw(Keyword::SelfUpper)
        )
    {
        return "";
    }

    if prev_role.is_generic_close() {
        return if matches!(
            curr_kind,
            TokenKind::LParen
                | TokenKind::LBracket
                | TokenKind::Dot
                | TokenKind::Question
                | TokenKind::Comma
                | TokenKind::Semi
                | TokenKind::Colon
                | TokenKind::RParen
                | TokenKind::RBracket
                | TokenKind::RBrace
        ) {
            ""
        } else {
            " "
        };
    }

    if prev_kind == TokenKind::LBrace {
        return if curr_kind == TokenKind::RBrace {
            ""
        } else {
            " "
        };
    }
    if curr_kind == TokenKind::LBrace {
        return " ";
    }
    if curr_kind == TokenKind::RBrace {
        return if prev_kind == TokenKind::LBrace {
            ""
        } else {
            " "
        };
    }
    if prev_kind == TokenKind::RBrace {
        return if matches!(
            curr_kind,
            TokenKind::LParen
                | TokenKind::LBracket
                | TokenKind::Dot
                | TokenKind::Question
                | TokenKind::Comma
                | TokenKind::Semi
                | TokenKind::Colon
                | TokenKind::RParen
                | TokenKind::RBracket
                | TokenKind::RBrace
        ) {
            ""
        } else {
            " "
        };
    }

    if curr_kind == TokenKind::StrStart {
        return if matches!(
            prev_kind,
            TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace
        ) {
            ""
        } else {
            " "
        };
    }

    if is_spaced_operator(prev_kind, angle_roles[curr_idx - 1], tokens, curr_idx - 1)
        || is_spaced_operator(curr_kind, angle_roles[curr_idx], tokens, curr_idx)
    {
        return " ";
    }

    if needs_separator_to_avoid_merge(prev_text, curr_text) {
        " "
    } else {
        ""
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum AngleRole {
    None,
    GenericOpen,
    GenericClose,
    GenericCloseClose,
}

impl AngleRole {
    fn is_generic_close(self) -> bool {
        matches!(self, AngleRole::GenericClose | AngleRole::GenericCloseClose)
    }
}

fn classify_angle_roles(tokens: &[&Token]) -> Vec<AngleRole> {
    let mut roles = vec![AngleRole::None; tokens.len()];
    for idx in 0..tokens.len() {
        if tokens[idx].kind != TokenKind::Lt || !plausible_generic_open(tokens, idx) {
            continue;
        }
        let Some(close_idx) = find_generic_close(tokens, idx) else {
            continue;
        };
        if !plausible_generic_follow(tokens, idx, close_idx) {
            continue;
        }
        mark_generic_angle_roles(&mut roles, tokens, idx, close_idx);
    }
    roles
}

fn classify_angle_roles_for_spacing(src: &str, tokens: &[&Token]) -> Vec<AngleRole> {
    let mut roles = classify_angle_roles(tokens);
    for idx in 0..tokens.len() {
        if roles[idx] != AngleRole::None
            || tokens[idx].kind != TokenKind::Lt
            || !plausible_generic_open(tokens, idx)
        {
            continue;
        }
        let Some(close_idx) = find_generic_close(tokens, idx) else {
            continue;
        };
        if tokens.get(close_idx + 1).map(|token| token.kind) != Some(TokenKind::Ident) {
            continue;
        }
        let Some(head) = tokens.get(idx.saturating_sub(1)) else {
            continue;
        };
        if !looks_like_named_type_head(src, head) {
            continue;
        }
        mark_generic_angle_roles(&mut roles, tokens, idx, close_idx);
    }
    roles
}

fn mark_generic_angle_roles(
    roles: &mut [AngleRole],
    tokens: &[&Token],
    open_idx: usize,
    close_idx: usize,
) {
    roles[open_idx] = AngleRole::GenericOpen;
    roles[close_idx] = match tokens[close_idx].kind {
        TokenKind::Shr => AngleRole::GenericCloseClose,
        _ => AngleRole::GenericClose,
    };
}

fn looks_like_named_type_head(src: &str, token: &Token) -> bool {
    if matches!(
        token.kind,
        TokenKind::Kw(Keyword::SelfUpper | Keyword::Extend)
    ) {
        return true;
    }
    if token.kind != TokenKind::Ident {
        return false;
    }
    tok_text(src, token)
        .chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_uppercase())
}

fn plausible_generic_open(tokens: &[&Token], idx: usize) -> bool {
    idx > 0
        && idx + 1 < tokens.len()
        && matches!(
            tokens[idx - 1].kind,
            TokenKind::Ident | TokenKind::Kw(Keyword::SelfUpper | Keyword::Extend)
        )
        && is_typeish_start(tokens[idx + 1].kind)
}

fn find_generic_close(tokens: &[&Token], open_idx: usize) -> Option<usize> {
    let mut depth = 1usize;
    let mut idx = open_idx + 1;
    while idx < tokens.len() {
        match tokens[idx].kind {
            TokenKind::Lt if plausible_generic_open(tokens, idx) => depth += 1,
            TokenKind::Gt => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(idx);
                }
            }
            TokenKind::Shr if depth >= 2 => {
                depth -= 2;
                if depth == 0 {
                    return Some(idx);
                }
            }
            TokenKind::Shr if depth == 1 => return Some(idx),
            TokenKind::Eof | TokenKind::Semi | TokenKind::LBrace | TokenKind::RBrace
                if depth > 0 =>
            {
                return None;
            }
            _ => {}
        }
        idx += 1;
    }
    None
}

fn plausible_generic_follow(tokens: &[&Token], open_idx: usize, close_idx: usize) -> bool {
    if matches!(
        tokens.get(open_idx.wrapping_sub(1)).map(|t| t.kind),
        Some(TokenKind::Kw(Keyword::Extend))
    ) {
        return true;
    }
    let Some(next) = tokens.get(close_idx + 1).map(|t| t.kind) else {
        return true;
    };
    if next == TokenKind::Plus && close_idx + 2 == tokens.len() {
        return true;
    }
    if matches!(
        next,
        TokenKind::LParen
            | TokenKind::LBrace
            | TokenKind::RParen
            | TokenKind::RBracket
            | TokenKind::Comma
            | TokenKind::Colon
            | TokenKind::Semi
            | TokenKind::Eq
            | TokenKind::FatArrow
            | TokenKind::Plus
            | TokenKind::Pipe
            | TokenKind::Question
            | TokenKind::Dot
            | TokenKind::Gt
            | TokenKind::Shr
            | TokenKind::Kw(Keyword::Async)
            | TokenKind::Kw(Keyword::As)
            | TokenKind::Kw(Keyword::Is)
    ) {
        return true;
    }

    false
}

fn is_typeish_start(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Ident
            | TokenKind::Underscore
            | TokenKind::LParen
            | TokenKind::LBracket
            | TokenKind::Star
            | TokenKind::Amp
            | TokenKind::Kw(Keyword::Null)
            | TokenKind::Kw(Keyword::SelfUpper)
            | TokenKind::Kw(Keyword::SelfLower)
    )
}

fn is_spaced_operator(
    kind: TokenKind,
    angle_role: AngleRole,
    tokens: &[&Token],
    idx: usize,
) -> bool {
    match kind {
        TokenKind::Eq
        | TokenKind::EqEq
        | TokenKind::BangEq
        | TokenKind::Lt
        | TokenKind::Gt
        | TokenKind::LtEq
        | TokenKind::GtEq
        | TokenKind::AmpAmp
        | TokenKind::PipePipe
        | TokenKind::Pipe
        | TokenKind::FatArrow => angle_role == AngleRole::None,
        TokenKind::Shl | TokenKind::Shr => angle_role == AngleRole::None,
        TokenKind::Plus
        | TokenKind::Minus
        | TokenKind::Star
        | TokenKind::Slash
        | TokenKind::Percent
        | TokenKind::Amp
        | TokenKind::Caret => {
            (idx > 0
                && idx + 1 < tokens.len()
                && is_value_end(tokens[idx - 1].kind)
                && is_value_start(tokens[idx + 1].kind))
                || (kind == TokenKind::Plus
                    && idx > 0
                    && idx + 1 == tokens.len()
                    && is_value_end(tokens[idx - 1].kind))
        }
        _ => false,
    }
}

fn is_value_end(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Ident
            | TokenKind::Underscore
            | TokenKind::Int { .. }
            | TokenKind::Float { .. }
            | TokenKind::Char
            | TokenKind::StrEnd
            | TokenKind::RParen
            | TokenKind::RBracket
            | TokenKind::RBrace
            | TokenKind::Kw(Keyword::True)
            | TokenKind::Kw(Keyword::False)
            | TokenKind::Kw(Keyword::Null)
            | TokenKind::Kw(Keyword::SelfLower)
            | TokenKind::Kw(Keyword::SelfUpper)
    )
}

fn is_value_start(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Ident
            | TokenKind::Underscore
            | TokenKind::Int { .. }
            | TokenKind::Float { .. }
            | TokenKind::Char
            | TokenKind::StrStart
            | TokenKind::LParen
            | TokenKind::LBracket
            | TokenKind::LBrace
            | TokenKind::Kw(Keyword::True)
            | TokenKind::Kw(Keyword::False)
            | TokenKind::Kw(Keyword::Null)
            | TokenKind::Kw(Keyword::SelfLower)
            | TokenKind::Kw(Keyword::SelfUpper)
    )
}

fn needs_separator_to_avoid_merge(prev: &str, curr: &str) -> bool {
    let Some(a) = prev.chars().last() else {
        return false;
    };
    let Some(b) = curr.chars().next() else {
        return false;
    };
    is_wordish(a) && is_wordish(b)
}

fn is_wordish(c: char) -> bool {
    c == '_' || c.is_alphanumeric()
}

/// The safety invariant: `fmt` may only change whitespace, never code or
/// comments. Re-lex both texts and require an identical token stream — same
/// kinds and same source text per token — then compare the ordinary-comment
/// trivia stream the parser normally drops. Returns `true` when formatting is
/// safe to apply.
pub fn token_stream_preserved(before: &str, after: &str) -> bool {
    let (ta, _) = lex(before, FileId(0));
    let (tb, _) = lex(after, FileId(0));
    if ta.len() != tb.len() {
        return false;
    }
    if !ta
        .iter()
        .zip(tb.iter())
        .all(|(x, y)| x.kind == y.kind && tok_text(before, x) == tok_text(after, y))
    {
        return false;
    }

    let (ca, ea) = lex_ordinary_comments(before, FileId(0));
    let (cb, eb) = lex_ordinary_comments(after, FileId(0));
    ea.is_empty()
        && eb.is_empty()
        && ca.len() == cb.len()
        && ca
            .iter()
            .zip(cb.iter())
            .all(|(x, y)| x.kind == y.kind && comment_text(before, x) == comment_text(after, y))
}

fn tok_text<'a>(src: &'a str, t: &crate::token::Token) -> &'a str {
    let r = t.span.range();
    src.get(r).unwrap_or("")
}

fn comment_text<'a>(src: &'a str, c: &crate::lexer::CommentTrivia) -> &'a str {
    let r = c.span.range();
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
    fn preserves_line_comment_text_in_safety_gate() {
        let src = "function f() {\nvar x = 1; // keep me\n}\n";
        let out = format_source(src);
        assert!(out.contains("// keep me"));
        assert!(token_stream_preserved(src, &out));
        assert!(!token_stream_preserved(
            src,
            "function f() {\n  var x = 1;\n}\n"
        ));
        assert!(!token_stream_preserved(
            src,
            "function f() {\n  var x = 1; // changed\n}\n"
        ));
    }

    #[test]
    fn preserves_nested_block_comment_text_in_safety_gate() {
        let src = "function f() {\n  /* outer /* inner */ done */\n}\n";
        let out = format_source(src);
        assert!(token_stream_preserved(src, &out));
        assert!(!token_stream_preserved(
            src,
            "function f() {\n  /* outer done */\n}\n"
        ));
    }

    #[test]
    fn normalizes_common_intra_line_spacing() {
        let src = "function f(a:i64,b:i64):i64{\nvar x=a+b*2;\nif x>=10&&a!=b{return x;}\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f(a: i64, b: i64): i64 {\n  var x = a + b * 2;\n  if x >= 10 && a != b { return x; }\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn normalizes_angle_spacing_for_generics_and_comparisons() {
        let src = "function id<T>(x:T):T{\nvar xs:List<Map<str,List<T>>> =List<Map<str,List<T>>>();\nif x<10||x>20{return x;}\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function id<T>(x: T): T {\n  var xs: List<Map<str, List<T>>> = List<Map<str, List<T>>>();\n  if x < 10 || x > 20 { return x; }\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn normalizes_async_closure_and_null_generic_spacing() {
        let src = "function value_of(r:Joined < i64 >|Panicked):i64{\nmatch r {Joined < i64 > j=>j.value,_=>-1}\n}\nfunction main():Future < null > async{\nvar h:JoinHandle < i64 > =Task.spawn(()async=>{null});\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function value_of(r: Joined<i64> | Panicked): i64 {\n  match r { Joined<i64> j => j.value, _ => -1 }\n}\nfunction main(): Future<null> async {\n  var h: JoinHandle<i64> = Task.spawn(() async => { null });\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn preserves_separate_nested_generic_closers() {
        let src = "function f(){\nvar xs:List<List<i64> > =List<List<i64> >();\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var xs: List<List<i64> > = List<List<i64> >();\n}\n"
        );
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn normalizes_code_before_line_comment_without_changing_comment_text() {
        let src = "function f(){\nvar x=1;//keep  exact\n}\n";
        let out = format_source(src);
        assert_eq!(out, "function f() {\n  var x = 1; //keep  exact\n}\n");
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn normalizes_string_and_char_adjacent_spacing() {
        let src = "function f(){\nvar s=\"a+b\";\nvar c='+';\nvar url=\"http://host/a/*not comment*/\";//keep exact\nvar msg=\"value ${x+1}!\";\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var s = \"a+b\";\n  var c = '+';\n  var url = \"http://host/a/*not comment*/\"; //keep exact\n  var msg = \"value ${x + 1 }!\";\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn leaves_interpolation_comments_verbatim() {
        let src = "function f(){\nvar msg=\"value ${x/*keep*/+1}\";\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var msg=\"value ${x/*keep*/+1}\";\n}\n"
        );
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn normalizes_code_around_inline_block_comments() {
        let src = "function f(){\nvar x=1;/*keep  exact*/var y=x+2;\nvar z=y*3; /* outer /* nested */ done */ //line  exact\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var x = 1; /*keep  exact*/ var y = x + 2;\n  var z = y * 3; /* outer /* nested */ done */ //line  exact\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn normalizes_multiline_block_comment_boundary_code() {
        let src = "function f(){\nvar x=1; /* open\n  comment\n*/var y=x+2;\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var x = 1; /* open\n  comment\n*/ var y = x + 2;\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_call_argument_lists() {
        let src = "function f(){\nvar total=combine(alpha,beta,gamma,delta,epsilon,zeta,eta,theta,iota,kappa,lambda,mu,nu);\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var total = combine(\n    alpha,\n    beta,\n    gamma,\n    delta,\n    epsilon,\n    zeta,\n    eta,\n    theta,\n    iota,\n    kappa,\n    lambda,\n    mu,\n    nu\n  );\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_function_parameter_lists_without_splitting_generics() {
        let src = "function build(first:Map<str,List<i64>>,second:Map<str,List<i64>>,third:Map<str,List<i64>>):i64{return 1;}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function build(\n  first: Map<str, List<i64>>,\n  second: Map<str, List<i64>>,\n  third: Map<str, List<i64>>\n): i64 { return 1; }\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_function_declaration_headers_as_fallback() {
        let src = "function very_long_function_name_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda(arg: i64): i64 { arg }\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function\n  very_long_function_name_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda(arg: i64): i64 { arg }\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_pub_function_declaration_headers_as_fallback() {
        let src = "pub function very_long_function_name_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda(arg: i64): i64 { arg }\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "pub function\n  very_long_function_name_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda(arg: i64): i64 { arg }\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_external_module_declarations_as_fallback() {
        let src = "mod very_long_module_name_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron_pi_rho_sigma;\npub mod very_long_module_name_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron_pi_rho_sigma;\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "mod\n  very_long_module_name_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron_pi_rho_sigma;\npub mod\n  very_long_module_name_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron_pi_rho_sigma;\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_inline_module_declarations_as_fallback() {
        let src = "pub mod very_long_module_name_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron_pi_rho_sigma { function value(): i64 { 1 } }\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "pub mod\n  very_long_module_name_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron_pi_rho_sigma { function value(): i64 { 1 } }\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_interface_declaration_headers_as_fallback() {
        let src = "interface VeryLongInterfaceNameAlphaBetaGammaDeltaEpsilonZetaEtaThetaIotaKappaLambdaMuNuXiOmicronPiRhoSigma {}\npub interface VeryLongInterfaceNameAlphaBetaGammaDeltaEpsilonZetaEtaThetaIotaKappaLambdaMuNuXiOmicronPiRhoSigma {}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "interface\n  VeryLongInterfaceNameAlphaBetaGammaDeltaEpsilonZetaEtaThetaIotaKappaLambdaMuNuXiOmicronPiRhoSigma {}\npub interface\n  VeryLongInterfaceNameAlphaBetaGammaDeltaEpsilonZetaEtaThetaIotaKappaLambdaMuNuXiOmicronPiRhoSigma {}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_extend_declaration_headers_as_fallback() {
        let src = "extend<T> VeryLongWrapperNameAlphaBetaGammaDeltaEpsilonZetaEtaThetaIotaKappaLambdaMuNuXiOmicronPiRhoSigma<T> { function value(self): i64 { 1 } }\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "extend\n  <T> VeryLongWrapperNameAlphaBetaGammaDeltaEpsilonZetaEtaThetaIotaKappaLambdaMuNuXiOmicronPiRhoSigma<T> { function value(self): i64 { 1 } }\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_struct_declaration_headers_as_fallback() {
        let src = "struct VeryLongStructNameAlphaBetaGammaDeltaEpsilonZetaEtaThetaIotaKappaLambdaMuNuXiOmicronPiRhoSigma;\npub struct VeryLongStructNameAlphaBetaGammaDeltaEpsilonZetaEtaThetaIotaKappaLambdaMuNuXiOmicronPiRhoSigma { value: i64 }\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "struct\n  VeryLongStructNameAlphaBetaGammaDeltaEpsilonZetaEtaThetaIotaKappaLambdaMuNuXiOmicronPiRhoSigma;\npub struct\n  VeryLongStructNameAlphaBetaGammaDeltaEpsilonZetaEtaThetaIotaKappaLambdaMuNuXiOmicronPiRhoSigma { value: i64 }\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_extern_struct_declaration_headers_as_fallback() {
        let src = "extern struct VeryLongForeignStructNameAlphaBetaGammaDeltaEpsilonZetaEtaThetaIotaKappaLambdaMuNuXiOmicronPiRhoSigma { value: i64 }\npub extern struct VeryLongForeignStructNameAlphaBetaGammaDeltaEpsilonZetaEtaThetaIotaKappaLambdaMuNuXiOmicronPiRhoSigma<T>(pub T);\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "extern struct\n  VeryLongForeignStructNameAlphaBetaGammaDeltaEpsilonZetaEtaThetaIotaKappaLambdaMuNuXiOmicronPiRhoSigma { value: i64 }\npub extern struct\n  VeryLongForeignStructNameAlphaBetaGammaDeltaEpsilonZetaEtaThetaIotaKappaLambdaMuNuXiOmicronPiRhoSigma<T>(pub T);\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_type_alias_declaration_headers_as_fallback() {
        let src = "type VeryLongAliasNameAlphaBetaGammaDeltaEpsilonZetaEtaThetaIotaKappaLambdaMuNuXiOmicronPiRhoSigma = i64;\npub type VeryLongAliasNameAlphaBetaGammaDeltaEpsilonZetaEtaThetaIotaKappaLambdaMuNuXiOmicronPiRhoSigma<T> = List<T>;\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "type\n  VeryLongAliasNameAlphaBetaGammaDeltaEpsilonZetaEtaThetaIotaKappaLambdaMuNuXiOmicronPiRhoSigma = i64;\npub type\n  VeryLongAliasNameAlphaBetaGammaDeltaEpsilonZetaEtaThetaIotaKappaLambdaMuNuXiOmicronPiRhoSigma<T> = List<T>;\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn leaves_long_function_type_alias_headers_unwrapped_as_non_declarations() {
        let src = "type Callback = function(alpha: Alpha, beta: Beta, gamma: Gamma, delta: Delta, epsilon: Epsilon): Result;\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "type Callback = function(\n  alpha: Alpha,\n  beta: Beta,\n  gamma: Gamma,\n  delta: Delta,\n  epsilon: Epsilon\n): Result;\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_generic_angle_lists() {
        let src = "type Many = Result<Alpha,Beta,Gamma,Delta,Epsilon,Zeta,Eta,Theta,Iota,Kappa,Lambda,Mu,Nu>;\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "type Many = Result<\n  Alpha,\n  Beta,\n  Gamma,\n  Delta,\n  Epsilon,\n  Zeta,\n  Eta,\n  Theta,\n  Iota,\n  Kappa,\n  Lambda,\n  Mu,\n  Nu\n>;\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_generic_angles_without_splitting_nested_generics_or_double_close() {
        let src = "type Many = Outer<Alpha,Beta,Inner<Gamma,Delta>,Inner<Epsilon,Zeta>,Inner<Eta,Theta>,Inner<Iota,Kappa>,Inner<Lambda,Mu>>;\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "type Many = Outer<\n  Alpha,\n  Beta,\n  Inner<Gamma, Delta>,\n  Inner<Epsilon, Zeta>,\n  Inner<Eta, Theta>,\n  Inner<Iota, Kappa>,\n  Inner<Lambda, Mu>>;\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_comparison_logical_chains_as_non_generic_angles() {
        let src = "function f(){\nvar ok=alpha<beta&&gamma<delta&&epsilon<zeta&&eta<theta&&iota<kappa&&lambda<mu&&nu<xi;\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var ok = alpha < beta\n    && gamma < delta\n    && epsilon < zeta\n    && eta < theta\n    && iota < kappa\n    && lambda < mu\n    && nu < xi;\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_type_alias_union_lists() {
        let src = "type Value = Alpha|Beta|Gamma|Delta|Epsilon|Zeta|Eta|Theta|Iota|Kappa|Lambda|Mu|Nu|Xi|Omicron;\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "type Value =\n  Alpha |\n  Beta |\n  Gamma |\n  Delta |\n  Epsilon |\n  Zeta |\n  Eta |\n  Theta |\n  Iota |\n  Kappa |\n  Lambda |\n  Mu |\n  Nu |\n  Xi |\n  Omicron;\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_type_alias_unions_without_splitting_nested_items() {
        let src = "type Handler = Result<Alpha|Beta,Gamma>|((Delta,Epsilon)=>Zeta)|Result<Eta|Theta,Iota>|((Kappa,Lambda)=>Mu)|Nu;\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "type Handler =\n  Result<Alpha | Beta, Gamma> |\n  ((Delta, Epsilon) => Zeta) |\n  Result<Eta | Theta, Iota> |\n  ((Kappa, Lambda) => Mu) |\n  Nu;\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_var_type_union_lists() {
        let src = "function f(){\nvar value: Alpha|Beta|Gamma|Delta|Epsilon|Zeta|Eta|Theta|Iota|Kappa|Lambda|Mu|Nu|Xi|Omicron = alpha;\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var value:\n    Alpha |\n    Beta |\n    Gamma |\n    Delta |\n    Epsilon |\n    Zeta |\n    Eta |\n    Theta |\n    Iota |\n    Kappa |\n    Lambda |\n    Mu |\n    Nu |\n    Xi |\n    Omicron = alpha;\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_var_type_unions_without_splitting_nested_items() {
        let src = "function f(){\nvar handler: Result<Alpha|Beta,Gamma>|((Delta,Epsilon)=>Zeta)|Result<Eta|Theta,Iota>|((Kappa,Lambda)=>Mu)|Nu = alpha;\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var handler:\n    Result<Alpha | Beta, Gamma> |\n    ((Delta, Epsilon) => Zeta) |\n    Result<Eta | Theta, Iota> |\n    ((Kappa, Lambda) => Mu) |\n    Nu = alpha;\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_semicolon_terminated_var_type_union_lists() {
        let src = "function f(){\nvar value: Alpha|Beta|Gamma|Delta|Epsilon|Zeta|Eta|Theta|Iota|Kappa|Lambda|Mu|Nu|Xi|Omicron;\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var value:\n    Alpha |\n    Beta |\n    Gamma |\n    Delta |\n    Epsilon |\n    Zeta |\n    Eta |\n    Theta |\n    Iota |\n    Kappa |\n    Lambda |\n    Mu |\n    Nu |\n    Xi |\n    Omicron;\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_function_parameter_type_union_lists() {
        let src = "function handle(value: Alpha|Beta|Gamma|Delta|Epsilon|Zeta|Eta|Theta|Iota|Kappa|Lambda|Mu|Nu|Xi|Omicron): i64 { 1 }\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function handle(value:\n  Alpha |\n  Beta |\n  Gamma |\n  Delta |\n  Epsilon |\n  Zeta |\n  Eta |\n  Theta |\n  Iota |\n  Kappa |\n  Lambda |\n  Mu |\n  Nu |\n  Xi |\n  Omicron): i64 { 1 }\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_parameter_type_unions_without_splitting_nested_items() {
        let src = "function handle(value: Result<Alpha|Beta,Gamma>|((Delta,Epsilon)=>Zeta)|Result<Eta|Theta,Iota>|((Kappa,Lambda)=>Mu)|Nu): i64 { 1 }\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function handle(value:\n  Result<Alpha | Beta, Gamma> |\n  ((Delta, Epsilon) => Zeta) |\n  Result<Eta | Theta, Iota> |\n  ((Kappa, Lambda) => Mu) |\n  Nu): i64 { 1 }\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_closure_parameter_type_union_lists() {
        let src = "function f(){\nvar normalize=(value: Alpha|Beta|Gamma|Delta|Epsilon|Zeta|Eta|Theta|Iota|Kappa|Lambda|Mu|Nu|Xi|Omicron) => value;\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var normalize = (value:\n    Alpha |\n    Beta |\n    Gamma |\n    Delta |\n    Epsilon |\n    Zeta |\n    Eta |\n    Theta |\n    Iota |\n    Kappa |\n    Lambda |\n    Mu |\n    Nu |\n    Xi |\n    Omicron) => value;\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_multiple_parameter_type_union_lists() {
        let src = "function handle(first: Alpha|Beta|Gamma|Delta|Epsilon|Zeta|Eta|Theta|Iota|Kappa|Lambda|Mu|Nu|Xi|Omicron, second: Rho|Sigma|Tau|Upsilon|Phi|Chi|Psi|Omega|One|Two|Three|Four|Five|Six|Seven): i64 { 1 }\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function handle(\n  first:\n    Alpha |\n    Beta |\n    Gamma |\n    Delta |\n    Epsilon |\n    Zeta |\n    Eta |\n    Theta |\n    Iota |\n    Kappa |\n    Lambda |\n    Mu |\n    Nu |\n    Xi |\n    Omicron,\n  second:\n    Rho |\n    Sigma |\n    Tau |\n    Upsilon |\n    Phi |\n    Chi |\n    Psi |\n    Omega |\n    One |\n    Two |\n    Three |\n    Four |\n    Five |\n    Six |\n    Seven\n): i64 { 1 }\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_mixed_parameter_type_union_lists() {
        let src = "function handle(first: i64, second: Result<Alpha|Beta,Gamma>|((Delta,Epsilon)=>Zeta)|Result<Eta|Theta,Iota>|((Kappa,Lambda)=>Mu)|Nu): i64 { 1 }\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function handle(\n  first: i64,\n  second:\n    Result<Alpha | Beta, Gamma> |\n    ((Delta, Epsilon) => Zeta) |\n    Result<Eta | Theta, Iota> |\n    ((Kappa, Lambda) => Mu) |\n    Nu\n): i64 { 1 }\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_closure_multiple_parameter_type_union_lists() {
        let src = "function f(){\nvar normalize=(first: Alpha|Beta|Gamma|Delta|Epsilon|Zeta|Eta|Theta, second: Iota|Kappa|Lambda|Mu|Nu|Xi|Omicron|Pi) => first;\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var normalize = (\n    first:\n      Alpha |\n      Beta |\n      Gamma |\n      Delta |\n      Epsilon |\n      Zeta |\n      Eta |\n      Theta,\n    second:\n      Iota |\n      Kappa |\n      Lambda |\n      Mu |\n      Nu |\n      Xi |\n      Omicron |\n      Pi\n  ) => first;\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_bitwise_or_chains_as_non_type_alias_unions() {
        let src = "function f(){\nvar mask=alpha|beta|gamma|delta|epsilon|zeta|eta|theta|iota|kappa|lambda|mu|nu|xi|omicron;\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var mask = alpha\n    | beta\n    | gamma\n    | delta\n    | epsilon\n    | zeta\n    | eta\n    | theta\n    | iota\n    | kappa\n    | lambda\n    | mu\n    | nu\n    | xi\n    | omicron;\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_function_return_union_lists() {
        let src = "function choose(): Alpha|Beta|Gamma|Delta|Epsilon|Zeta|Eta|Theta|Iota|Kappa|Lambda|Mu|Nu|Xi|Omicron { return alpha; }\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function choose():\n  Alpha |\n  Beta |\n  Gamma |\n  Delta |\n  Epsilon |\n  Zeta |\n  Eta |\n  Theta |\n  Iota |\n  Kappa |\n  Lambda |\n  Mu |\n  Nu |\n  Xi |\n  Omicron { return alpha; }\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_interface_method_return_unions_without_splitting_nested_items() {
        let src = "interface Service {\nfunction fetch(self): Result<Alpha|Beta,Gamma>|((Delta,Epsilon)=>Zeta)|Result<Eta|Theta,Iota>|((Kappa,Lambda)=>Mu)|Nu;\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "interface Service {\n  function fetch(self):\n    Result<Alpha | Beta, Gamma> |\n    ((Delta, Epsilon) => Zeta) |\n    Result<Eta | Theta, Iota> |\n    ((Kappa, Lambda) => Mu) |\n    Nu;\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_interface_member_lists() {
        let src = "interface Service { function start(self): i64; function stop(self): i64; function reset(self): i64; function status(self): i64; function configure(self, alpha: Alpha, beta: Beta): i64; }\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "interface Service {\n  function start(self): i64;\n  function stop(self): i64;\n  function reset(self): i64;\n  function status(self): i64;\n  function configure(self, alpha: Alpha, beta: Beta): i64;\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_interface_members_without_splitting_default_method_bodies() {
        let src = "interface Lifecycle { function start(self): i64; function stop(self): i64; function ready(self): bool { var code = self.start(); code > 0 } }\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "interface Lifecycle {\n  function start(self): i64;\n  function stop(self): i64;\n  function ready(self): bool { var code = self.start(); code > 0 }\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_extend_members_without_splitting_method_bodies() {
        let src = "extend Widget: Renderable { function render(self): str { self.name() } function width(self): i64 { var base = self.left(); base + self.right() } function height(self): i64 { self.top() + self.bottom() } }\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "extend Widget: Renderable {\n  function render(self): str { self.name() }\n  function width(self): i64 { var base = self.left(); base + self.right() }\n  function height(self): i64 { self.top() + self.bottom() }\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_list_literals() {
        let src = "function f(){\nvar xs=[alpha,beta,gamma,delta,epsilon,zeta,eta,theta,iota,kappa,lambda,mu,nu,xi,omicron,pi,rho];\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var xs = [\n    alpha,\n    beta,\n    gamma,\n    delta,\n    epsilon,\n    zeta,\n    eta,\n    theta,\n    iota,\n    kappa,\n    lambda,\n    mu,\n    nu,\n    xi,\n    omicron,\n    pi,\n    rho\n  ];\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_list_literals_without_splitting_nested_calls() {
        let src = "function f(){\nvar xs=[pair(alpha,beta),pair(gamma,delta),pair(epsilon,zeta),pair(eta,theta),pair(iota,kappa),pair(lambda,mu)];\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var xs = [\n    pair(alpha, beta),\n    pair(gamma, delta),\n    pair(epsilon, zeta),\n    pair(eta, theta),\n    pair(iota, kappa),\n    pair(lambda, mu)\n  ];\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_map_literals_with_string_keys() {
        let src = "function f(){\nvar m={\"alpha\":alpha,\"beta\":beta,\"gamma\":gamma,\"delta\":delta,\"epsilon\":epsilon,\"zeta\":zeta,\"eta\":eta,\"theta\":theta};\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var m = {\n    \"alpha\": alpha,\n    \"beta\": beta,\n    \"gamma\": gamma,\n    \"delta\": delta,\n    \"epsilon\": epsilon,\n    \"zeta\": zeta,\n    \"eta\": eta,\n    \"theta\": theta\n  };\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_map_literals_without_splitting_nested_maps_or_spreads() {
        let src = "function f(){\nvar merged={..base,1:{10:alpha,11:beta},2:{20:gamma,21:delta},3:{30:epsilon,31:zeta},4:{40:eta,41:theta}};\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var merged = {\n    ..base,\n    1: { 10: alpha, 11: beta },\n    2: { 20: gamma, 21: delta },\n    3: { 30: epsilon, 31: zeta },\n    4: { 40: eta, 41: theta }\n  };\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_struct_literals() {
        let src = "function f(){\nvar p=Point{x:alpha,y:beta,z:gamma,w:delta,a:epsilon,b:zeta,c:eta,d:theta,e:iota,f:kappa,g:lambda};\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var p = Point {\n    x: alpha,\n    y: beta,\n    z: gamma,\n    w: delta,\n    a: epsilon,\n    b: zeta,\n    c: eta,\n    d: theta,\n    e: iota,\n    f: kappa,\n    g: lambda\n  };\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_struct_literals_without_splitting_nested_structs() {
        let src = "function f(){\nvar p=Outer{first:Inner{x:alpha,y:beta},second:Inner{x:gamma,y:delta},third:Inner{x:epsilon,y:zeta},fourth:Inner{x:eta,y:theta}};\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var p = Outer {\n    first: Inner { x: alpha, y: beta },\n    second: Inner { x: gamma, y: delta },\n    third: Inner { x: epsilon, y: zeta },\n    fourth: Inner { x: eta, y: theta }\n  };\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_record_struct_declarations() {
        let src = "pub struct Packet { pub alpha: Alpha, beta: Beta, gamma: Gamma, delta: Delta, epsilon: Epsilon, zeta: Zeta, eta: Eta, theta: Theta, iota: Iota }\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "pub struct Packet {\n  pub alpha: Alpha,\n  beta: Beta,\n  gamma: Gamma,\n  delta: Delta,\n  epsilon: Epsilon,\n  zeta: Zeta,\n  eta: Eta,\n  theta: Theta,\n  iota: Iota\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_record_struct_declarations_without_splitting_nested_field_types() {
        let src = "extern struct Layout<T> { pub first: Map<str,List<T>>, second: Result<Alpha,Gamma>, third: (Delta,Epsilon)=>Zeta, fourth: Omega }\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "extern struct Layout<T> {\n  pub first: Map<str, List<T>>,\n  second: Result<Alpha, Gamma>,\n  third: (Delta, Epsilon) => Zeta,\n  fourth: Omega\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_record_field_type_union_lists() {
        let src = "struct Packet { value: Alpha|Beta|Gamma|Delta|Epsilon|Zeta|Eta|Theta|Iota|Kappa|Lambda|Mu|Nu|Xi|Omicron }\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "struct Packet {\n  value:\n    Alpha |\n    Beta |\n    Gamma |\n    Delta |\n    Epsilon |\n    Zeta |\n    Eta |\n    Theta |\n    Iota |\n    Kappa |\n    Lambda |\n    Mu |\n    Nu |\n    Xi |\n    Omicron\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_record_field_type_unions_without_splitting_nested_items() {
        let src = "pub struct Packet { pub value: Result<Alpha|Beta,Gamma>|((Delta,Epsilon)=>Zeta)|Result<Eta|Theta,Iota>|((Kappa,Lambda)=>Mu)|Nu, tag: i64 }\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "pub struct Packet {\n  pub value:\n    Result<Alpha | Beta, Gamma> |\n    ((Delta, Epsilon) => Zeta) |\n    Result<Eta | Theta, Iota> |\n    ((Kappa, Lambda) => Mu) |\n    Nu,\n  tag: i64\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_interface_superinterface_lists() {
        let src = "interface Renderable: Alpha+Beta+Gamma+Delta+Epsilon+Zeta+Eta+Theta+Iota+Kappa+Lambda+Mu {\nfunction render(self): str;\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "interface Renderable:\n  Alpha +\n  Beta +\n  Gamma +\n  Delta +\n  Epsilon +\n  Zeta +\n  Eta +\n  Theta +\n  Iota +\n  Kappa +\n  Lambda +\n  Mu {\n  function render(self): str;\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_extend_interface_lists_without_splitting_generic_interfaces() {
        let src = "extend Result<Alpha,Beta>: Mapper<Alpha,Beta>+Reducer<Gamma,Delta>+Renderer<Epsilon,Zeta>+Debuggable<Eta,Theta>+Serializable<Iota,Kappa> {\nfunction id(self): i64 { 1 }\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "extend Result<Alpha, Beta>:\n  Mapper<Alpha, Beta> +\n  Reducer<Gamma, Delta> +\n  Renderer<Epsilon, Zeta> +\n  Debuggable<Eta, Theta> +\n  Serializable<Iota, Kappa> {\n  function id(self): i64 { 1 }\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_generic_bound_lists() {
        let src = "function dump<T: Alpha+Beta+Gamma+Delta+Epsilon+Zeta+Eta+Theta+Iota+Kappa+Lambda+Mu>(x: T): T { x }\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function dump<T:\n  Alpha +\n  Beta +\n  Gamma +\n  Delta +\n  Epsilon +\n  Zeta +\n  Eta +\n  Theta +\n  Iota +\n  Kappa +\n  Lambda +\n  Mu>(x: T): T { x }\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_generic_bounds_without_splitting_nested_generic_items() {
        let src = "interface Cache<T: Renderable<Alpha+Beta,Gamma>+Serializable<Delta+Epsilon,Zeta>+Cloneable+Debuggable+Hashable> { function get(self): T; }\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "interface Cache<T:\n  Renderable<Alpha + Beta, Gamma> +\n  Serializable<Delta + Epsilon, Zeta> +\n  Cloneable +\n  Debuggable +\n  Hashable> { function get(self): T; }\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_multiple_generic_bound_lists() {
        let src = "function dump<T: Alpha+Beta+Gamma+Delta+Epsilon+Zeta+Eta+Theta, U: Iota+Kappa+Lambda+Mu+Nu+Xi+Omicron+Pi>(x: T, y: U): T { x }\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function dump<\n  T:\n    Alpha +\n    Beta +\n    Gamma +\n    Delta +\n    Epsilon +\n    Zeta +\n    Eta +\n    Theta,\n  U:\n    Iota +\n    Kappa +\n    Lambda +\n    Mu +\n    Nu +\n    Xi +\n    Omicron +\n    Pi\n>(x: T, y: U): T { x }\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_mixed_generic_bound_lists() {
        let src = "function dump<T, U: Alpha+Beta+Gamma+Delta+Epsilon+Zeta+Eta+Theta>(x: T, y: U): T { x }\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function dump<\n  T,\n  U:\n    Alpha +\n    Beta +\n    Gamma +\n    Delta +\n    Epsilon +\n    Zeta +\n    Eta +\n    Theta\n>(x: T, y: U): T { x }\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_multiple_generic_bounds_without_splitting_nested_items() {
        let src = "interface Cache<T: Renderable<Alpha+Beta,Gamma>+Serializable<Delta+Epsilon,Zeta>+Cloneable, U: Loader<Eta+Theta,Iota>+Storable<Kappa+Lambda,Mu>+Debuggable> { function get(self): T; }\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "interface Cache<\n  T:\n    Renderable<Alpha + Beta, Gamma> +\n    Serializable<Delta + Epsilon, Zeta> +\n    Cloneable,\n  U:\n    Loader<Eta + Theta, Iota> +\n    Storable<Kappa + Lambda, Mu> +\n    Debuggable\n> { function get(self): T; }\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_arrow_closure_return_union_lists() {
        let src = "function f(){\nvar classify=(x: i64): Alpha|Beta|Gamma|Delta|Epsilon|Zeta|Eta|Theta|Iota|Kappa|Lambda|Mu|Nu|Xi|Omicron => alpha;\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var classify = (x: i64):\n    Alpha |\n    Beta |\n    Gamma |\n    Delta |\n    Epsilon |\n    Zeta |\n    Eta |\n    Theta |\n    Iota |\n    Kappa |\n    Lambda |\n    Mu |\n    Nu |\n    Xi |\n    Omicron => alpha;\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_async_arrow_closure_return_unions_without_splitting_nested_items() {
        let src = "function f(){\nvar classify=(x: i64): Result<Alpha|Beta,Gamma>|((Delta,Epsilon)=>Zeta)|Result<Eta|Theta,Iota>|((Kappa,Lambda)=>Mu)|Nu async => alpha;\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var classify = (x: i64):\n    Result<Alpha | Beta, Gamma> |\n    ((Delta, Epsilon) => Zeta) |\n    Result<Eta | Theta, Iota> |\n    ((Kappa, Lambda) => Mu) |\n    Nu async => alpha;\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn leaves_function_type_pipes_unwrapped_as_non_arrow_closure_returns() {
        let src = "function f(){\nvar handler: (Result<Alpha|Beta,Gamma>,Result<Delta|Epsilon,Zeta>,Result<Eta|Theta,Iota>,Result<Kappa|Lambda,Mu>) => Omega = make_handler();\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var handler: (\n    Result<Alpha | Beta, Gamma>,\n    Result<Delta | Epsilon, Zeta>,\n    Result<Eta | Theta, Iota>,\n    Result<Kappa | Lambda, Mu>\n  ) => Omega = make_handler();\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn leaves_long_generic_value_plus_lines_unwrapped_as_non_bounds() {
        let src = "function f(){\nvar total=combine<Alpha>(alpha+beta+gamma+delta+epsilon+zeta+eta+theta+iota+kappa+lambda+mu+nu);\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var total = combine<Alpha>(alpha + beta + gamma + delta + epsilon + zeta + eta + theta + iota + kappa + lambda + mu + nu);\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_additive_chains_as_non_interface_bounds() {
        let src = "function f(){\nvar total=alpha+beta-gamma+delta-epsilon+zeta+eta-theta+iota+kappa-lambda+mu+nu-xi+omicron;\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var total = alpha\n    + beta\n    - gamma\n    + delta\n    - epsilon\n    + zeta\n    + eta\n    - theta\n    + iota\n    + kappa\n    - lambda\n    + mu\n    + nu\n    - xi\n    + omicron;\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_multiplicative_chains() {
        let src = "function f(){\nvar total=alpha*beta/gamma%delta*epsilon/zeta*eta/theta%iota*kappa/lambda*mu/nu%xi*omicron;\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var total = alpha\n    * beta\n    / gamma\n    % delta\n    * epsilon\n    / zeta\n    * eta\n    / theta\n    % iota\n    * kappa\n    / lambda\n    * mu\n    / nu\n    % xi\n    * omicron;\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn leaves_multiplicative_terms_with_top_level_addition_unwrapped() {
        let src = "function f(){\nvar total=alpha+beta*gamma*delta*epsilon*zeta*eta*theta*iota*kappa*lambda*mu*nu*xi*omicron*pi;\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var total = alpha + beta * gamma * delta * epsilon * zeta * eta * theta * iota * kappa * lambda * mu * nu * xi * omicron * pi;\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_shift_chains() {
        let src = "function f(){\nvar shifted=alpha<<beta>>gamma<<delta>>epsilon<<zeta>>eta<<theta>>iota<<kappa>>lambda<<mu>>nu<<xi>>omicron;\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var shifted = alpha\n    << beta\n    >> gamma\n    << delta\n    >> epsilon\n    << zeta\n    >> eta\n    << theta\n    >> iota\n    << kappa\n    >> lambda\n    << mu\n    >> nu\n    << xi\n    >> omicron;\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_shift_chains_before_additive_operands() {
        let src = "function f(){\nvar shifted=alpha+beta<<gamma-delta>>epsilon+zeta<<eta-theta>>iota+kappa<<lambda-mu>>nu+xi<<omicron-pi;\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var shifted = alpha + beta\n    << gamma - delta\n    >> epsilon + zeta\n    << eta - theta\n    >> iota + kappa\n    << lambda - mu\n    >> nu + xi\n    << omicron - pi;\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn leaves_shift_terms_with_top_level_bitwise_and_unwrapped() {
        let src = "function f(){\nvar mask=alpha&beta<<gamma<<delta<<epsilon<<zeta<<eta<<theta<<iota<<kappa<<lambda<<mu<<nu<<xi<<omicron;\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var mask = alpha & beta << gamma << delta << epsilon << zeta << eta << theta << iota << kappa << lambda << mu << nu << xi << omicron;\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_cast_chains() {
        let src = "function f(){\nvar value=source as Alpha as Beta as Gamma as Delta as Epsilon as Zeta as Eta as Theta as Iota as Kappa as Lambda;\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var value = source\n    as Alpha\n    as Beta\n    as Gamma\n    as Delta\n    as Epsilon\n    as Zeta\n    as Eta\n    as Theta\n    as Iota\n    as Kappa\n    as Lambda;\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_cast_chains_without_splitting_generic_targets() {
        let src = "function f(){\nvar value=source as Box<Alpha,Beta> as Result<Gamma<Delta,Epsilon>,Zeta> as Pair<Theta,Iota> as Omega<Kappa,Lambda>;\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var value = source\n    as Box<Alpha, Beta>\n    as Result<Gamma<Delta, Epsilon>, Zeta>\n    as Pair<Theta, Iota>\n    as Omega<Kappa, Lambda>;\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn leaves_cast_terms_with_top_level_addition_unwrapped() {
        let src = "function f(){\nvar value=left as Alpha as Beta+right as Gamma as Delta+third as Epsilon as Zeta+fourth as Eta as Theta;\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var value = left as Alpha as Beta\n    + right as Gamma as Delta\n    + third as Epsilon as Zeta\n    + fourth as Eta as Theta;\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_bitwise_and_chains() {
        let src = "function f(){\nvar mask=alpha&beta&gamma&delta&epsilon&zeta&eta&theta&iota&kappa&lambda&mu&nu&xi&omicron&pi;\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var mask = alpha\n    & beta\n    & gamma\n    & delta\n    & epsilon\n    & zeta\n    & eta\n    & theta\n    & iota\n    & kappa\n    & lambda\n    & mu\n    & nu\n    & xi\n    & omicron\n    & pi;\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_bitwise_and_chains_before_multiplicative_operands() {
        let src = "function f(){\nvar mask=alpha*beta&gamma/delta&epsilon%zeta&eta*theta&iota/kappa&lambda%mu&nu*xi&omicron/pi;\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var mask = alpha * beta\n    & gamma / delta\n    & epsilon % zeta\n    & eta * theta\n    & iota / kappa\n    & lambda % mu\n    & nu * xi\n    & omicron / pi;\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_bitwise_xor_chains_before_bitwise_and_operands() {
        let src = "function f(){\nvar mask=alpha&beta^gamma&delta^epsilon&zeta^eta&theta^iota&kappa^lambda&mu^nu&xi^omicron&pi;\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var mask = alpha & beta\n    ^ gamma & delta\n    ^ epsilon & zeta\n    ^ eta & theta\n    ^ iota & kappa\n    ^ lambda & mu\n    ^ nu & xi\n    ^ omicron & pi;\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_bitwise_xor_chains() {
        let src = "function f(){\nvar mask=alpha^beta^gamma^delta^epsilon^zeta^eta^theta^iota^kappa^lambda^mu^nu^xi^omicron^pi;\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var mask = alpha\n    ^ beta\n    ^ gamma\n    ^ delta\n    ^ epsilon\n    ^ zeta\n    ^ eta\n    ^ theta\n    ^ iota\n    ^ kappa\n    ^ lambda\n    ^ mu\n    ^ nu\n    ^ xi\n    ^ omicron\n    ^ pi;\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_bitwise_or_chains_before_bitwise_xor_operands() {
        let src = "function f(){\nvar mask=alpha^beta|gamma^delta|epsilon^zeta|eta^theta|iota^kappa|lambda^mu|nu^xi|omicron^pi;\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var mask = alpha ^ beta\n    | gamma ^ delta\n    | epsilon ^ zeta\n    | eta ^ theta\n    | iota ^ kappa\n    | lambda ^ mu\n    | nu ^ xi\n    | omicron ^ pi;\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_comparison_expressions_before_bitwise_operands() {
        let src = "function f(){\nvar ok=alpha|beta|gamma|delta|epsilon|zeta|eta|theta==iota|kappa|lambda|mu|nu|xi|omicron|pi;\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var ok = alpha | beta | gamma | delta | epsilon | zeta | eta | theta\n    == iota | kappa | lambda | mu | nu | xi | omicron | pi;\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_greater_comparisons_without_confusing_generic_angles() {
        let src = "function f(){\nvar ok=compute<Alpha,Beta>(first_value,second_value,third_value)>other<Gamma,Delta>(fourth_value,fifth_value,sixth_value);\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var ok = compute<Alpha, Beta>(first_value, second_value, third_value)\n    > other<Gamma, Delta>(fourth_value, fifth_value, sixth_value);\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn leaves_chained_comparisons_unwrapped_as_parser_invalid() {
        let src = "function f(){\nvar ok=alpha==beta==gamma==delta==epsilon==zeta==eta==theta==iota==kappa==lambda;\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var ok = alpha == beta == gamma == delta == epsilon == zeta == eta == theta == iota == kappa == lambda;\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_var_initializers_as_fallback() {
        let src = "function f(){\nvar generated=generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron;\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var generated =\n    generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron;\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_typed_long_var_initializers_as_fallback() {
        let src = "function f(){\nvar generated: str=generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi;\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var generated: str =\n    generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi;\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_return_expressions_as_fallback() {
        let src = "function f(){\nreturn generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron;\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  return\n    generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron;\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_break_expressions_as_fallback() {
        let src = "function f(){\nloop {\nbreak generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron;\n}\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  loop {\n    break\n      generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron;\n  }\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_match_arm_bodies_as_fallback() {
        let src = "function f(){\nvar out=match value {\nJoinedResult result => generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron,\n_=>0\n};\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var out = match value {\n    JoinedResult result =>\n      generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron,\n    _ => 0\n  };\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_arrow_closure_bodies_as_fallback() {
        let src = "function f(){\nvar worker=(value:i64):i64=>generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron;\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var worker = (value: i64): i64 =>\n    generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron;\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_trailing_closure_bodies_as_fallback() {
        let src = "function f(){\nThread.spawn { async => generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron; };\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  Thread.spawn { async =>\n    generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron; };\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));

        let typed = "function f(){\nstate.lock { value: Counter async => generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron; };\n}\n";
        let typed_out = format_source(typed);
        assert_eq!(
            typed_out,
            "function f() {\n  state.lock { value: Counter async =>\n    generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron; };\n}\n"
        );
        assert_eq!(format_source(&typed_out), typed_out);
        assert!(token_stream_preserved(typed, &typed_out));
    }

    #[test]
    fn wraps_long_implicit_trailing_closure_bodies_as_fallback() {
        let src = "function f(){\nitems.each { generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron; };\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  items.each {\n    generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron; };\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_anonymous_function_expression_headers_as_fallback() {
        let src = "function f(){\ncall(function(value: VeryLongTypeAlphaBetaGammaDeltaEpsilonZetaEtaThetaIotaKappaLambdaMuNuXiOmicron): VeryLongReturnAlphaBetaGammaDeltaEpsilonZetaEtaThetaIota async { value });\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  call(function\n    (value: VeryLongTypeAlphaBetaGammaDeltaEpsilonZetaEtaThetaIotaKappaLambdaMuNuXiOmicron): VeryLongReturnAlphaBetaGammaDeltaEpsilonZetaEtaThetaIota async { value });\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_async_block_bodies_as_fallback() {
        let src = "function f(){\ndrive(async { generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron; });\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  drive(async {\n      generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron; });\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_block_expression_bodies_as_fallback() {
        let src = "function f(){\ndrive({ generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron; });\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  drive({\n      generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron; });\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_macro_block_bodies_as_fallback() {
        let src = "function f(){\nvar x=@AsBlock { generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron; };\nvar y=@Trace(\"op\") { generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron; };\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var x = @AsBlock {\n    generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron; };\n  var y = @Trace(\"op\") {\n    generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron; };\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_loop_bodies_as_fallback() {
        let src = "function f(){\nloop { generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron; }\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  loop {\n    generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron; }\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_else_bodies_as_fallback() {
        let src = "function f(){\nif ready {\nshort();\n} else { generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron; }\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  if ready {\n    short();\n  } else {\n    generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron; }\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_if_bodies_as_fallback() {
        let src = "function f(){\nif ready { generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron; }\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  if ready {\n    generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron; }\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_while_bodies_as_fallback() {
        let src = "function f(){\nwhile ready { generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron; }\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  while ready {\n    generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron; }\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_for_bodies_as_fallback() {
        let src = "function f(){\nfor item in items { generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron; }\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  for item in items {\n    generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron; }\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_function_bodies_as_fallback() {
        let src = "function f(){ return generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron; }\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  return generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron; }\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_test_bodies_as_fallback() {
        let src = "test \"short\" { generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron; }\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "test \"short\" {\n  generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron; }\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_module_bodies_as_fallback() {
        let src = "mod short { generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron; }\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "mod short {\n  generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron; }\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_await_operands_as_fallback() {
        let src = "function f(){\nawait generated_future_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron;\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  await\n    generated_future_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron;\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_spawn_operands_as_fallback() {
        let src = "function f(){\nspawn generated_worker_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron;\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  spawn\n    generated_worker_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron;\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_test_declaration_headers_as_fallback() {
        let src = "test \"formatter_test_name_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron_pi\" {}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "test\n  \"formatter_test_name_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron_pi\" {}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_bench_declaration_headers_as_fallback() {
        let src = "bench \"formatter_bench_name_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron_pi\" {}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "bench\n  \"formatter_bench_name_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron_pi\" {}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_assignment_expressions_as_fallback() {
        let src = "function f(){\ngenerated=generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron;\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  generated =\n    generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron;\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_field_assignment_expressions_as_fallback() {
        let src = "function f(){\nself.generated_field_value=generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi;\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  self.generated_field_value =\n    generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi;\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_for_iterators_as_fallback() {
        let src = "function f(){\nfor item in generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron {}\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  for item in\n    generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron {}\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_for_await_iterators_as_fallback() {
        let src = "function f(){\nfor await item in generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi {}\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  for await item in\n    generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi {}\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_while_conditions_as_fallback() {
        let src = "function f(){\nwhile generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron {}\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  while\n    generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron {}\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_if_conditions_as_fallback() {
        let src = "function f(){\nif generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron {}\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  if\n    generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron {}\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_else_if_conditions_as_fallback() {
        let src = "function f(){\nif ready {}\nelse if generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron {}\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  if ready {}\n  else if\n    generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron {}\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_match_scrutinees_as_fallback() {
        let src = "function f(){\nvar out=match generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron {_=>1};\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var out = match\n    generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron { _ => 1 };\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_match_guard_conditions_as_fallback() {
        let src = "function f(){\nvar out=match kind {\n0 if generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron=>alpha,\n_=>omega\n};\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var out = match kind {\n    0 if\n      generated_value_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron => alpha,\n    _ => omega\n  };\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn leaves_single_long_logical_for_iterator_unwrapped() {
        let src = "function f(){\nfor item in alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa&&lambda_mu_nu_xi_omicron_pi_rho_sigma_tau {}\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  for item in alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa && lambda_mu_nu_xi_omicron_pi_rho_sigma_tau {}\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn leaves_single_long_logical_while_condition_unwrapped() {
        let src = "function f(){\nwhile alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa&&lambda_mu_nu_xi_omicron_pi_rho_sigma_tau {}\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  while alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa && lambda_mu_nu_xi_omicron_pi_rho_sigma_tau {}\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn leaves_single_long_logical_if_condition_unwrapped() {
        let src = "function f(){\nif alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa&&lambda_mu_nu_xi_omicron_pi_rho_sigma_tau {}\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  if alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa && lambda_mu_nu_xi_omicron_pi_rho_sigma_tau {}\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn leaves_single_long_logical_match_scrutinee_unwrapped() {
        let src = "function f(){\nvar out=match alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa&&lambda_mu_nu_xi_omicron_pi_rho_sigma_tau {_=>1};\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var out = match alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa && lambda_mu_nu_xi_omicron_pi_rho_sigma_tau { _ => 1 };\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn leaves_single_long_logical_match_guard_unwrapped() {
        let src = "function f(){\nvar out=match kind {\n0 if alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa&&lambda_mu_nu_xi_omicron_pi_rho_sigma_tau=>alpha,\n_=>omega\n};\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var out = match kind {\n    0 if alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa && lambda_mu_nu_xi_omicron_pi_rho_sigma_tau => alpha,\n    _ => omega\n  };\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn leaves_single_long_logical_break_expression_unwrapped() {
        let src = "function f(){\nloop {\nbreak alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa&&lambda_mu_nu_xi_omicron_pi_rho_sigma_tau;\n}\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  loop {\n    break alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa && lambda_mu_nu_xi_omicron_pi_rho_sigma_tau;\n  }\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn leaves_single_long_logical_await_operand_unwrapped() {
        let src = "function f(){\nawait alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa&&lambda_mu_nu_xi_omicron_pi_rho_sigma_tau;\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  await alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa && lambda_mu_nu_xi_omicron_pi_rho_sigma_tau;\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn leaves_single_long_logical_assignment_expression_unwrapped() {
        let src = "function f(){\ngenerated=alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa&&lambda_mu_nu_xi_omicron_pi_rho_sigma_tau;\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  generated = alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa && lambda_mu_nu_xi_omicron_pi_rho_sigma_tau;\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn leaves_single_long_logical_return_expression_unwrapped() {
        let src = "function f(){\nreturn alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa&&lambda_mu_nu_xi_omicron_pi_rho_sigma_tau;\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  return alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa && lambda_mu_nu_xi_omicron_pi_rho_sigma_tau;\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_match_arm_lists_after_call_scrutinee() {
        let src = "function f(){\nvar out=match classify(input) {0=>alpha,1=>beta,2=>gamma,3=>delta,4=>epsilon,5=>zeta,6=>eta,7=>theta,_=>omega};\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var out = match classify(input) {\n    0 => alpha,\n    1 => beta,\n    2 => gamma,\n    3 => delta,\n    4 => epsilon,\n    5 => zeta,\n    6 => eta,\n    7 => theta,\n    _ => omega\n  };\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_match_arms_without_splitting_nested_structs() {
        let src = "function f(){\nvar out=match kind {0=>Point{x:alpha,y:beta},1=>Point{x:gamma,y:delta},2=>Point{x:epsilon,y:zeta},_=>Point{x:eta,y:theta}};\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var out = match kind {\n    0 => Point { x: alpha, y: beta },\n    1 => Point { x: gamma, y: delta },\n    2 => Point { x: epsilon, y: zeta },\n    _ => Point { x: eta, y: theta }\n  };\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_named_import_lists_without_touching_path_string() {
        let src = "pub import { Alpha,Beta as Renamed,Gamma,Delta,Epsilon,Zeta,Eta,Theta,Iota,Kappa,Lambda,Mu,Nu } from \"std:io\";\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "pub import {\n  Alpha,\n  Beta as Renamed,\n  Gamma,\n  Delta,\n  Epsilon,\n  Zeta,\n  Eta,\n  Theta,\n  Iota,\n  Kappa,\n  Lambda,\n  Mu,\n  Nu\n} from \"std:io\";\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn named_import_list_state_stops_after_same_line_from_path() {
        let src = "import { Alpha,Beta,Gamma,Delta,Epsilon,Zeta,Eta,Theta,Iota,Kappa,Lambda,Mu,Nu } from \"std:io\";\ninterface Service {\nfunction start(self): i64;\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "import {\n  Alpha,\n  Beta,\n  Gamma,\n  Delta,\n  Epsilon,\n  Zeta,\n  Eta,\n  Theta,\n  Iota,\n  Kappa,\n  Lambda,\n  Mu,\n  Nu\n} from \"std:io\";\ninterface Service {\n  function start(self): i64;\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_named_import_list_closing_paths() {
        let src = "pub import { Alpha,Beta as Renamed,Gamma,Delta,Epsilon,Zeta,Eta,Theta,Iota,Kappa,Lambda,Mu,Nu } from \"pkg:alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron_pi_rho_sigma_tau\";\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "pub import {\n  Alpha,\n  Beta as Renamed,\n  Gamma,\n  Delta,\n  Epsilon,\n  Zeta,\n  Eta,\n  Theta,\n  Iota,\n  Kappa,\n  Lambda,\n  Mu,\n  Nu\n}\n  from \"pkg:alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron_pi_rho_sigma_tau\";\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_named_import_paths_as_fallback() {
        let src = "import { Logger } from \"pkg:alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron_pi_rho_sigma_tau\";\npub import { Helper as RenamedHelper } from \"self:alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron\";\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "import { Logger }\n  from \"pkg:alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron_pi_rho_sigma_tau\";\npub import { Helper as RenamedHelper }\n  from \"self:alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron\";\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_single_argument_attributes_as_fallback() {
        let src = "@Symbol(\"very_very_long_symbol_name_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron\")\nextern function inflate_init(code: i32);\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "@Symbol(\n  \"very_very_long_symbol_name_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron\"\n)\nextern function inflate_init(code: i32);\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_ambient_import_paths_as_fallback() {
        let src = "import \"pkg:alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron_pi_rho_sigma_tau\";\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "import\n  \"pkg:alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron_pi_rho_sigma_tau\";\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_namespace_import_paths_as_fallback() {
        let src = "pub import \"pkg:alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron_pi_rho_sigma_tau\" as LongPkg;\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "pub import\n  \"pkg:alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa_lambda_mu_nu_xi_omicron_pi_rho_sigma_tau\" as LongPkg;\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_extern_type_declarations_as_fallback() {
        let src = "extern type VeryLongOpaqueForeignHandleAlphaBetaGammaDeltaEpsilonZetaEtaThetaIotaKappaLambdaMuNuXiOmicronPiRho;\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "extern type\n  VeryLongOpaqueForeignHandleAlphaBetaGammaDeltaEpsilonZetaEtaThetaIotaKappaLambdaMuNuXiOmicronPiRho;\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_pub_extern_type_declarations_as_fallback() {
        let src = "pub extern type VeryLongOpaqueForeignHandleAlphaBetaGammaDeltaEpsilonZetaEtaThetaIotaKappaLambdaMuNuXiOmicronPiRho;\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "pub extern type\n  VeryLongOpaqueForeignHandleAlphaBetaGammaDeltaEpsilonZetaEtaThetaIotaKappaLambdaMuNuXiOmicronPiRho;\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_extern_var_declarations_as_fallback() {
        let src = "extern var very_long_foreign_global_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa: VeryLongForeignRuntimeCounterHandleAlphaBetaGammaDelta;\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "extern var\n  very_long_foreign_global_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa: VeryLongForeignRuntimeCounterHandleAlphaBetaGammaDelta;\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_pub_extern_var_declarations_as_fallback() {
        let src = "pub extern var very_long_foreign_global_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa: AtomicForeignRuntimeCounterHandle;\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "pub extern var\n  very_long_foreign_global_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa: AtomicForeignRuntimeCounterHandle;\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_extern_function_declarations_as_fallback() {
        let src = "extern function very_long_foreign_function_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa(arg: i64): VeryLongForeignRuntimeResultHandle;\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "extern function\n  very_long_foreign_function_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa(arg: i64): VeryLongForeignRuntimeResultHandle;\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_pub_extern_function_declarations_as_fallback() {
        let src = "pub extern function very_long_foreign_function_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa(arg: i64): VeryLongForeignRuntimeResultHandle;\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "pub extern function\n  very_long_foreign_function_alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa(arg: i64): VeryLongForeignRuntimeResultHandle;\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_string_argument_lists() {
        let src = "function f(){\nvar total=combine(\"literal\",alpha,beta,gamma,delta,epsilon,zeta,eta,theta,iota,kappa,lambda,mu,nu);\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var total = combine(\n    \"literal\",\n    alpha,\n    beta,\n    gamma,\n    delta,\n    epsilon,\n    zeta,\n    eta,\n    theta,\n    iota,\n    kappa,\n    lambda,\n    mu,\n    nu\n  );\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_method_chains() {
        let src = "function f() {\nvar result=service.alpha().beta(one,two).gamma<Delta,Epsilon>().delta().epsilon().zeta().eta().theta().iota().kappa().lambda();\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var result = service\n    .alpha()\n    .beta(one, two)\n    .gamma<Delta, Epsilon>()\n    .delta()\n    .epsilon()\n    .zeta()\n    .eta()\n    .theta()\n    .iota()\n    .kappa()\n    .lambda();\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_separate_field_access_additive_chains_as_non_method_chains() {
        let src = "function f() {\nvar total=alpha.beta+gamma.delta+epsilon.zeta+eta.theta+iota.kappa+lambda.mu+nu.xi+omicron.pi+rho.sigma;\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var total = alpha.beta\n    + gamma.delta\n    + epsilon.zeta\n    + eta.theta\n    + iota.kappa\n    + lambda.mu\n    + nu.xi\n    + omicron.pi\n    + rho.sigma;\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn wraps_long_logical_chains() {
        let src = "function f() {\nvar ok=alpha&&beta(one,two)&&gamma<Delta,Epsilon>(three,four)||delta.epsilon()&&zeta&&eta(theta,iota)&&kappa;\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var ok = alpha\n    && beta(one, two)\n    && gamma<Delta, Epsilon>(three, four)\n    || delta.epsilon()\n    && zeta\n    && eta(theta, iota)\n    && kappa;\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn leaves_single_long_logical_expression_unwrapped_as_non_chain() {
        let src = "function f() {\nvar ok=alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa&&lambda_mu_nu_xi_omicron_pi_rho_sigma_tau;\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var ok = alpha_beta_gamma_delta_epsilon_zeta_eta_theta_iota_kappa && lambda_mu_nu_xi_omicron_pi_rho_sigma_tau;\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn leaves_comment_and_string_lines_unwrapped() {
        let src = "function f(){\nvar url=\"http://host/really/really/really/really/really/really/really/really/long/path\"; //keep exact\n}\n";
        let out = format_source(src);
        assert_eq!(
            out,
            "function f() {\n  var url = \"http://host/really/really/really/really/really/really/really/really/long/path\"; //keep exact\n}\n"
        );
        assert_eq!(format_source(&out), out);
        assert!(token_stream_preserved(src, &out));
    }

    #[test]
    fn trailing_newline_normalized() {
        assert_eq!(format_source("var x = 1;"), "var x = 1;\n");
        assert_eq!(format_source("var x = 1;\n\n\n"), "var x = 1;\n");
        assert_eq!(format_source(""), "");
    }
}
