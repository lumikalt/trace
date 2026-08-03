//! A minimal language server (diagnostics, go-to-definition, hover) over
//! stdio, entered via `trace --lsp`. Reuses the exact `lex -> parse ->
//! resolve -> effects -> types` pipeline `main.rs` drives for the CLI, so
//! there is only ever one place that knows how to compile a `.tr` file.
//!
//! The compiler is single-file (no imports across `.tr` files exist yet),
//! so every definition a request could resolve to lives in the SAME
//! document as the request itself — go-to-definition never needs to
//! construct a URI for a different file, only echo the request's own.
//!
//! `Position.character` is a UTF-16 code-unit offset, per the LSP spec's
//! default (and, in practice, the ONLY encoding `vscode-languageclient`
//! actually supports as of this writing — it hardcodes `positionEncodings:
//! ['utf-16']` in what it advertises and rejects any `initialize` result
//! that claims otherwise, so declaring `PositionEncodingKind::UTF8` here
//! — cheaper, and spec-legal since LSP 3.17 — was tried and rejected by
//! the real client outright; see `LineIndex` for the resulting byte-offset
//! <-> UTF-16-code-unit conversion this compiler's byte-offset `Span`s
//! need on every position in and out).
//!
//! Each pass here mirrors `main.rs`'s own early-return-on-error chain:
//! diagnostics accumulate from whichever phases ran, but a phase after the
//! first one with errors never runs (its `Resolution`/`Types` would be
//! built on an already-invalid AST). Concretely: a file with a parse error
//! gets diagnostics but no go-to-definition/hover at all (no `Resolution`
//! exists yet); a file with only type errors still gets go-to-definition
//! (needs only `Resolution`) but hover without type info (needs `Types`).

use crate::ast::{Ast, Expr, ExprId, FnKind, Item, effects_str};
use crate::lexer::Span;
use crate::resolve::{DefId, DefKind, Resolution};
use crate::types::Types;
use crate::{effects, lexer, parser, resolve, types};
use lsp_server::{
    Connection, ExtractError, Message, Notification as ServerNotification,
    Request as ServerRequest, RequestId, Response,
};
use lsp_types::notification::{
    DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, Notification,
    PublishDiagnostics,
};
use lsp_types::request::{GotoDefinition, HoverRequest};
use lsp_types::{
    Diagnostic, DiagnosticSeverity, GotoDefinitionParams, GotoDefinitionResponse, Hover,
    HoverContents, HoverParams, HoverProviderCapability, Location, MarkedString, MarkupContent,
    MarkupKind, OneOf, Position, PositionEncodingKind, PublishDiagnosticsParams, Range,
    ServerCapabilities, TextDocumentSyncCapability, TextDocumentSyncKind, Uri,
};
use std::collections::HashMap;
use std::error::Error;

pub fn run() -> std::process::ExitCode {
    let (connection, io_threads) = Connection::stdio();
    let capabilities = ServerCapabilities {
        position_encoding: Some(PositionEncodingKind::UTF16),
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        definition_provider: Some(OneOf::Left(true)),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        ..Default::default()
    };
    let init_result = connection
        .initialize(serde_json::to_value(&capabilities).expect("capabilities always serialize"));
    if init_result.is_err() {
        // Client disconnected (or sent something malformed) before we ever
        // got to a real request — nothing to clean up, just exit quietly.
        return std::process::ExitCode::SUCCESS;
    }

    // Keyed by `Uri::as_str()` rather than `Uri` itself: `Uri` carries a
    // caching `Cell` internally (interior mutability), which clippy's
    // `mutable_key_type` correctly refuses to trust as a `HashMap` key
    // even though its `Hash`/`Eq` impls are stable (both delegate to
    // `as_str()`, never the cache) — every `Uri` needed for a RESPONSE
    // still comes straight from that request's own params, never
    // reconstructed from this map, so the string key costs nothing.
    let mut docs: HashMap<String, String> = HashMap::new();
    // `main_loop` takes `connection` BY VALUE so it drops (closing its
    // `sender` channel) before `io_threads.join()` below — the writer
    // thread's `for msg in receiver` loop only ends when every `Sender`
    // is dropped, so joining while `connection` (and its `sender`) is
    // still alive out here would deadlock forever on a clean shutdown.
    let result = main_loop(connection, &mut docs);
    if io_threads.join().is_err() || result.is_err() {
        if let Err(e) = result {
            eprintln!("trace --lsp: {e}");
        }
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}

fn main_loop(
    connection: Connection,
    docs: &mut HashMap<String, String>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    for msg in &connection.receiver {
        match msg {
            Message::Request(req) => {
                if connection.handle_shutdown(&req)? {
                    return Ok(());
                }
                handle_request(&connection, docs, req)?;
            }
            Message::Notification(note) => {
                handle_notification(&connection, docs, note)?;
            }
            Message::Response(_) => {
                // We never send requests of our own, so nothing replies here.
            }
        }
    }
    Ok(())
}

/// `req.extract` consumes `req` and hands it back on a method mismatch, so
/// each request is tried against one candidate type at a time — the same
/// dispatch idiom `lsp-server`'s own docs use.
fn cast_request<R>(req: ServerRequest) -> Result<(RequestId, R::Params), ServerRequest>
where
    R: lsp_types::request::Request,
{
    match req.extract(R::METHOD) {
        Ok(it) => Ok(it),
        Err(ExtractError::MethodMismatch(req)) => Err(req),
        Err(ExtractError::JsonError { method, error }) => {
            panic!("invalid params for {method}: {error}")
        }
    }
}

fn cast_notification<N>(note: ServerNotification) -> Result<N::Params, ServerNotification>
where
    N: Notification,
{
    match note.extract(N::METHOD) {
        Ok(it) => Ok(it),
        Err(ExtractError::MethodMismatch(note)) => Err(note),
        Err(ExtractError::JsonError { method, error }) => {
            panic!("invalid params for {method}: {error}")
        }
    }
}

fn handle_request(
    connection: &Connection,
    docs: &HashMap<String, String>,
    req: ServerRequest,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let req = match cast_request::<HoverRequest>(req) {
        Ok((id, params)) => {
            let result = hover(docs, params);
            connection
                .sender
                .send(Message::Response(Response::new_ok(id, result)))?;
            return Ok(());
        }
        Err(req) => req,
    };
    let req = match cast_request::<GotoDefinition>(req) {
        Ok((id, params)) => {
            let result = goto_definition(docs, params);
            connection
                .sender
                .send(Message::Response(Response::new_ok(id, result)))?;
            return Ok(());
        }
        Err(req) => req,
    };
    // Unknown method: no response at all is the documented behavior for a
    // request the server doesn't advertise support for in its capabilities.
    let _ = req;
    Ok(())
}

fn handle_notification(
    connection: &Connection,
    docs: &mut HashMap<String, String>,
    note: ServerNotification,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let note = match cast_notification::<DidOpenTextDocument>(note) {
        Ok(params) => {
            let uri = params.text_document.uri;
            docs.insert(uri.as_str().to_string(), params.text_document.text);
            publish_diagnostics(connection, &uri, &docs[uri.as_str()])?;
            return Ok(());
        }
        Err(note) => note,
    };
    let note = match cast_notification::<DidChangeTextDocument>(note) {
        Ok(params) => {
            let uri = params.text_document.uri;
            // Full sync only (see capabilities above): the last change event
            // carries the document's entire new text, not an incremental
            // edit, so there is nothing to apply on top of the old text.
            if let Some(change) = params.content_changes.into_iter().next_back() {
                docs.insert(uri.as_str().to_string(), change.text);
            }
            if let Some(text) = docs.get(uri.as_str()) {
                publish_diagnostics(connection, &uri, text)?;
            }
            return Ok(());
        }
        Err(note) => note,
    };
    let note = match cast_notification::<DidCloseTextDocument>(note) {
        Ok(params) => {
            let uri = params.text_document.uri;
            docs.remove(uri.as_str());
            // Clear the closed document's diagnostics from the editor's
            // Problems panel rather than leaving stale ones behind.
            connection
                .sender
                .send(Message::Notification(ServerNotification::new(
                    PublishDiagnostics::METHOD.to_string(),
                    PublishDiagnosticsParams::new(uri, Vec::new(), None),
                )))?;
            return Ok(());
        }
        Err(note) => note,
    };
    let _ = note;
    Ok(())
}

fn publish_diagnostics(
    connection: &Connection,
    uri: &Uri,
    src: &str,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let index = LineIndex::new(src);
    let diagnostics = compile(src, &index).diagnostics;
    let params = PublishDiagnosticsParams::new(uri.clone(), diagnostics, None);
    connection
        .sender
        .send(Message::Notification(ServerNotification::new(
            PublishDiagnostics::METHOD.to_string(),
            params,
        )))?;
    Ok(())
}

/// Maps byte offsets (this compiler's `Span`) to/from LSP `Position`s,
/// whose `character` is a UTF-16 code-unit offset within the line (see
/// this module's own doc comment for why UTF-16, not the cheaper UTF-8
/// byte offset). Every `offset`/`character` this compiler ever hands in
/// sits on a UTF-8 char boundary (lexer/parser spans never split a
/// multi-byte char), so the `&self.src[a..b]` slicing below never panics.
struct LineIndex<'a> {
    src: &'a str,
    /// Byte offset of the start of each line; `line_starts[0] == 0`.
    line_starts: Vec<usize>,
}

impl<'a> LineIndex<'a> {
    fn new(src: &'a str) -> LineIndex<'a> {
        let mut line_starts = vec![0];
        line_starts.extend(
            src.bytes()
                .enumerate()
                .filter(|(_, b)| *b == b'\n')
                .map(|(i, _)| i + 1),
        );
        LineIndex { src, line_starts }
    }

    /// The byte range of line `line`'s own content, excluding its
    /// terminating `\n` (if any — the last line may have none).
    fn line_bytes(&self, line: usize) -> std::ops::Range<usize> {
        let start = self.line_starts[line];
        let end = match self.line_starts.get(line + 1) {
            Some(&next) => next - 1, // back up over the '\n'
            None => self.src.len(),
        };
        start..end
    }

    fn position(&self, offset: usize) -> Position {
        let line = match self.line_starts.binary_search(&offset) {
            Ok(l) => l,
            Err(l) => l - 1,
        };
        let line_start = self.line_starts[line];
        let character = self.src[line_start..offset].encode_utf16().count() as u32;
        Position {
            line: line as u32,
            character,
        }
    }

    fn offset(&self, pos: Position) -> usize {
        let line = (pos.line as usize).min(self.line_starts.len() - 1);
        let bytes = self.line_bytes(line);
        let line_text = &self.src[bytes.clone()];
        let mut utf16_count = 0u32;
        for (byte_idx, ch) in line_text.char_indices() {
            if utf16_count >= pos.character {
                return bytes.start + byte_idx;
            }
            utf16_count += ch.len_utf16() as u32;
        }
        // `pos.character` reaches past the line's real content (an editor
        // can send this right after a delete-to-end-of-line edit) — clamp
        // to the end of the line rather than reading into the next one.
        bytes.end
    }

    fn range(&self, span: &Span) -> Range {
        Range {
            start: self.position(span.start),
            end: self.position(span.end),
        }
    }
}

/// Best-effort compilation result: every phase that ran contributes its
/// errors to `diagnostics`, but (matching `main.rs`'s own early-return
/// chain) a phase only runs at all when every phase before it had zero
/// errors — see this module's own doc comment.
struct Compiled {
    diagnostics: Vec<Diagnostic>,
    ast: Option<Ast>,
    res: Option<Resolution>,
    ty: Option<Types>,
}

fn compile(src: &str, index: &LineIndex) -> Compiled {
    let mut diagnostics = Vec::new();
    let mut push = |span: &Span, message: &str| {
        diagnostics.push(Diagnostic {
            range: index.range(span),
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("trace".to_string()),
            message: message.to_string(),
            ..Default::default()
        });
    };

    let (tokens, lex_errors) = lexer::lex(src);
    for err in &lex_errors {
        push(&err.span, "unrecognized character(s)");
    }
    if !lex_errors.is_empty() {
        return Compiled {
            diagnostics,
            ast: None,
            res: None,
            ty: None,
        };
    }

    let (ast, parse_errors) = parser::parse(src, &tokens);
    for err in &parse_errors {
        push(&err.span, &err.message);
    }
    if !parse_errors.is_empty() {
        return Compiled {
            diagnostics,
            ast: Some(ast),
            res: None,
            ty: None,
        };
    }

    let (res, resolve_errors) = resolve::resolve(&ast);
    for err in &resolve_errors {
        push(&err.span, &err.message);
    }
    if !resolve_errors.is_empty() {
        return Compiled {
            diagnostics,
            ast: Some(ast),
            res: Some(res),
            ty: None,
        };
    }

    let (_fx, effect_errors) = effects::check(&ast, &res);
    for err in &effect_errors {
        push(&err.span, &err.message);
    }
    if !effect_errors.is_empty() {
        return Compiled {
            diagnostics,
            ast: Some(ast),
            res: Some(res),
            ty: None,
        };
    }

    let (ty, type_errors) = types::check(&ast, &res);
    for err in &type_errors {
        push(&err.span, &err.message);
    }
    Compiled {
        diagnostics,
        ast: Some(ast),
        res: Some(res),
        ty: Some(ty),
    }
}

/// The smallest `Expr::Ident` span containing `offset`, if any — used as
/// the "what's under the cursor" query for both definition and hover.
/// Identifiers never nest inside one another, so in practice at most one
/// span ever matches; the narrowest-first scan is a defensive tie-break,
/// not a load-bearing requirement. A flat linear scan over `expr_spans` is
/// fine at these file sizes — this runs on every hover/definition request,
/// which is already gated by a full recompile on every keystroke (see
/// `handle_notification`'s `didChange`), so it's not the bottleneck.
fn ident_at(ast: &Ast, offset: usize) -> Option<ExprId> {
    let mut best: Option<(ExprId, usize)> = None;
    for (i, span) in ast.expr_spans.iter().enumerate() {
        if span.start <= offset && offset <= span.end && matches!(ast.exprs[i], Expr::Ident(_)) {
            let len = span.end - span.start;
            if best.is_none_or(|(_, best_len)| len < best_len) {
                best = Some((ExprId(i as u32), len));
            }
        }
    }
    best.map(|(id, _)| id)
}

/// A `reads {a, b}`/`writes {a}` row argument (a plain `Name`, never an
/// `Expr::Ident`) whose own span contains `offset`, if any — the third
/// case `thing_at` dispatches to, alongside a live use and a declaration
/// site. Backed by `resolve::Resolution::effect_arg_defs`, populated once
/// by `check_effect_args` at resolve time rather than re-deriving a
/// name -> def lookup here (this module never has scope information of
/// its own to do that with). A flat linear scan, same "fine at these file
/// sizes" rationale `ident_at`'s own scan already relies on.
fn effect_arg_at(res: &Resolution, offset: usize) -> Option<(Span, DefId)> {
    for (span, &def) in &res.effect_arg_defs {
        if span.start <= offset && offset <= span.end {
            return Some((span.clone(), def));
        }
    }
    None
}

/// What's under the cursor, tried in this order: a live `Expr::Ident`
/// *use* of a def (`Some(expr)`, so a per-expression type is also
/// available from `expr_tys`); a `reads`/`writes` row argument (`None` —
/// it's a plain `Name`, not an `Expr::Ident`, but still a real reference to
/// the def it names, same as a use); or the def's own *declaration site*
/// (`None` — a `reg`/`in`/`out`/`fn`/... name is never itself an
/// `Expr::Ident` either, it's plain data on an `Item`). The returned
/// `Span` is always the SITE actually under the cursor (an expression's
/// own span, a row argument's own span, or the declaration's own span) —
/// never assumed from which branch matched, so hover/go-to-definition
/// always highlight the right text even though only the use-site branch
/// has a real `ExprId` to also key `expr_tys` with.
fn thing_at(ast: &Ast, res: &Resolution, offset: usize) -> Option<(Span, Option<ExprId>, DefId)> {
    if let Some(expr) = ident_at(ast, offset)
        && let Some(&def) = res.expr_defs.get(&expr)
    {
        return Some((ast.expr_spans[expr.0 as usize].clone(), Some(expr), def));
    }
    if let Some((span, def)) = effect_arg_at(res, offset) {
        return Some((span, None, def));
    }
    let mut best: Option<(DefId, usize)> = None;
    for (i, def) in res.defs.iter().enumerate() {
        if def.span.is_empty() {
            continue; // builtins (see `resolve::Def::span`'s own doc)
        }
        if def.span.start <= offset && offset <= def.span.end {
            let len = def.span.end - def.span.start;
            if best.is_none_or(|(_, best_len)| len < best_len) {
                best = Some((DefId(i as u32), len));
            }
        }
    }
    best.map(|(id, _)| (res.def(id).span.clone(), None, id))
}

fn goto_definition(
    docs: &HashMap<String, String>,
    params: GotoDefinitionParams,
) -> Option<GotoDefinitionResponse> {
    let uri = params.text_document_position_params.text_document.uri;
    let src = docs.get(uri.as_str())?;
    let index = LineIndex::new(src);
    let compiled = compile(src, &index);
    let ast = compiled.ast.as_ref()?;
    let res = compiled.res.as_ref()?;
    let offset = index.offset(params.text_document_position_params.position);
    let (_, _, def) = thing_at(ast, res, offset)?;
    let def = res.def(def);
    if def.span.is_empty() {
        // Builtins carry an empty span (see `resolve::Def::span`'s own
        // doc) — nothing to jump to.
        return None;
    }
    Some(GotoDefinitionResponse::Scalar(Location::new(
        uri,
        index.range(&def.span),
    )))
}

fn hover(docs: &HashMap<String, String>, params: HoverParams) -> Option<Hover> {
    let uri = params.text_document_position_params.text_document.uri;
    let src = docs.get(uri.as_str())?;
    let index = LineIndex::new(src);
    let compiled = compile(src, &index);
    let ast = compiled.ast.as_ref()?;
    let offset = index.offset(params.text_document_position_params.position);
    // Tried before `thing_at`/`res`, and needs neither: an effect keyword
    // (`reads`/`combines`/...) is pure syntax on `Item::Rule`/`Item::Fn`,
    // never an `Expr::Ident` and never given a `resolve::Def` either (see
    // `effect_hover`'s own doc comment), so it needs a third lookup path
    // independent of both `ident_at` and the `Def::span` fallback —
    // works even on a file with resolve errors, same as diagnostics do.
    if let Some(hover) = effect_hover(ast, &index, offset) {
        return Some(hover);
    }
    let res = compiled.res.as_ref()?;
    let (range, expr, def_id) = thing_at(ast, res, offset)?;
    let def = res.def(def_id);
    // A fn/spec/impl's "type" isn't one scalar `Ty` the way a reg or a
    // local's is — it's a whole signature (params, return, effects) — so
    // it gets its own richer rendering: a fenced code block matching the
    // VS Code extension's own language id (`trace`), syntax-highlighted
    // the same as the source itself, plus a placeholder description line
    // since there are no doc comments to pull a real one from yet.
    if let Some(sig) = fn_signature(ast, res, src, def_id) {
        let desc = match def.kind {
            DefKind::Spec => "A spec.",
            DefKind::Impl => "An impl.",
            _ => "A function.",
        };
        return Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: format!("```trace\n{sig}\n```\n\n{desc}"),
            }),
            range: Some(index.range(&range)),
        });
    }
    // A use site's type comes from `expr_tys` (per-expression, since the
    // same def can be read at different widths through absorption); a
    // declaration site has no `ExprId` of its own to key that map with, so
    // it falls back to the def-keyed `local_tys`/`state_tys` maps instead
    // — between them, every def with a scalar type is covered (a rule/
    // module/... def has neither and just shows its kind, same as an
    // untyped use site already does below).
    let ty = match expr {
        Some(expr) => compiled.ty.as_ref().and_then(|ty| ty.expr_tys.get(&expr)),
        None => compiled.ty.as_ref().and_then(|ty| {
            ty.local_tys
                .get(&def_id)
                .or_else(|| ty.state_tys.get(&def_id))
        }),
    };
    let text = match ty {
        Some(ty) => format!("`{}: {ty}` — {}", def.name, def.kind.describe()),
        None => format!("`{}` — {}", def.name, def.kind.describe()),
    };
    Some(Hover {
        contents: HoverContents::Scalar(MarkedString::String(text)),
        range: Some(index.range(&range)),
    })
}

/// Hover for an effect keyword (`reads`/`writes`/`combines`/`sequences`/
/// `elaborates`/`fails`/`chooses`) inside a `<...>` list, if `offset` sits
/// inside one's own name span. An effect's `Name` is plain syntax on
/// `Item::Rule`/`Item::Fn` (`ast.rs`'s `Effect` struct) — never an
/// `Expr::Ident` (so `ident_at` never finds it) and never given a
/// `resolve::Def` either, unlike every other declaration this module
/// hovers (nothing ever needs to reference an effect the way a call
/// references a fn, or a read references a reg) — so this is its own
/// third lookup path, independent of `thing_at`. `ast.items` is a flat
/// arena (a module's own nesting is expressed through `ItemId`
/// cross-references stored ON items, not real Rust-level tree nesting),
/// so one linear pass already reaches every rule/fn regardless of which
/// module contains it — same "fine at these file sizes" scan `ident_at`
/// already relies on.
fn effect_hover(ast: &Ast, index: &LineIndex, offset: usize) -> Option<Hover> {
    for item in &ast.items {
        let effects = match item {
            Item::Rule { effects, .. } | Item::Fn { effects, .. } => effects,
            _ => continue,
        };
        for effect in effects {
            if effect.name.span.start <= offset && offset <= effect.name.span.end {
                let value = effect_doc(&effect.name.text)?;
                return Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value,
                    }),
                    range: Some(index.range(&effect.name.span)),
                });
            }
        }
    }
    None
}

/// A description and a runnable example for each real effect keyword,
/// straight from DESIGN.md's own "Effects" section — kept in sync by hand,
/// the same way `effects_str`'s rendering and the parser's own effect
/// grammar already have to independently agree on the same 7 names.
/// `None` for anything else (a typo, or a name this dispatch hasn't been
/// taught yet) — `effect_hover` just shows nothing rather than guessing.
fn effect_doc(name: &str) -> Option<String> {
    let (desc, example): (&str, &str) = match name {
        "combines" => (
            "Total, pure, and always terminates — lowers to a plain combinational \
             expression, never registers. Loops over circuit values are rejected; only \
             an elaboration-time-bounded loop is legal here.",
            "Parity(x : [8]) : [1] <combines> {\n    return x[0] ^ x[1] ^ x[2] ^ x[3] ^ \
             x[4] ^ x[5] ^ x[6] ^ x[7]\n}",
        ),
        "sequences" => (
            "Spans more than one cycle. `tick` marks a cycle boundary; each cycle is \
             still its own one-cycle transaction — there is no cross-cycle rollback.",
            "Rmw(addr : [8]) <sequences, reads {mem}, writes {mem}> {\n    let v = \
             mem[addr]\n    tick\n    mem[addr] := v + 1\n}",
        ),
        "elaborates" => (
            "Runs once, before synthesis, to build the circuit. Recursion and dynamic \
             allocation are legal here, and only here.",
            "AdderTree(xs : list[wire[32]]) : wire[32] <elaborates> {\n    if len(xs) = \
             1 { return xs[0] }\n    let mid = len(xs) / 2\n    return \
             Add(AdderTree(xs[..mid]), AdderTree(xs[mid..]))\n}",
        ),
        "fails" => (
            "Marks code that can fail: a guard `?`, a fifo operation, or a call to \
             failing code. Inferred bottom-up through the call graph; must be declared \
             explicitly wherever the computed result is true, not silently left off.",
            "Classify(x : [8]) : [2] <combines, fails> {\n    (x <> 0)?\n    return \
             clog2(x)\n}",
        ),
        "chooses" => (
            "Marks spec-only nondeterminism: `|` and `any` become free variables for a \
             model checker. Only a `spec` may declare it; synthesizing code with it is \
             a type error.",
            "spec AnyGrant(reqs : [N]) : [clog2(N)] <combines, chooses, fails> {\n    \
             let i = any(0..N-1)\n    reqs[i]?\n    return i\n}",
        ),
        "reads" => (
            "The set of state (`reg`/`mem`/`fifo`/`in`) this rule or function reads. \
             Inferred by the compiler; stating it asserts an interface — overstating is \
             legal (a conservative claim is sound), understating is an error.",
            "rule refill <reads {pc, mem}, writes {ir}> {\n    ir := mem[pc]\n}",
        ),
        "writes" => (
            "The set of state (`reg`/`mem`/`fifo`) this rule or function writes. \
             Inferred by the compiler; stating it asserts an interface — overstating is \
             legal (a conservative claim is sound), understating is an error.",
            "rule refill <reads {pc, mem}, writes {ir}> {\n    ir := mem[pc]\n}",
        ),
        _ => return None,
    };
    Some(format!(
        "```trace\n<{name}>\n```\n\n{desc}\n\n```trace\n{example}\n```"
    ))
}

/// Renders a fn/spec/impl's own declared signature as it's written in
/// source (`fn Outer(x : [8]) : [8] <combines>`), by slicing each
/// parameter/return type annotation's own span directly out of `src`
/// rather than re-deriving a `Ty` and reformatting it — the exact text the
/// user wrote is unambiguous and never needs to handle `Ty::Unknown`/
/// generic-parameter display edge cases a `Ty`-based rendering would.
/// `None` for anything that isn't a fn/spec/impl def, or (defensively) if
/// `res.item_defs` somehow has no entry for one that is — the caller falls
/// back to the plain `name — kind` hover in either case.
fn fn_signature(ast: &Ast, res: &Resolution, src: &str, def_id: DefId) -> Option<String> {
    let mut item_id = None;
    for (&iid, &did) in &res.item_defs {
        if did == def_id {
            item_id = Some(iid);
            break;
        }
    }
    let item_id = item_id?;
    let Item::Fn {
        name,
        kind,
        params,
        ret,
        effects,
        ..
    } = ast.item(item_id)
    else {
        return None;
    };
    // `fn`/`FnKind::Fn`'s own bare name at item position is ALL source
    // syntax needs (see `parser.rs`'s `parse_item`, dispatched on a bare
    // `Ident` with no leading keyword) — `ast.rs`'s own debug dump prints
    // a synthetic `fn ` prefix for readability there, but hover renders
    // exactly what's legal to write, so only `spec`/`impl` (which DO
    // consume a real keyword token) get one here.
    let keyword = match kind {
        FnKind::Fn => "",
        FnKind::Spec => "spec ",
        FnKind::Impl { .. } => "impl ",
    };
    let params = params
        .iter()
        .map(|p| {
            format!(
                "{} : {}",
                p.name,
                &src[ast.expr_spans[p.ty.0 as usize].clone()]
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let mut sig = format!("{keyword}{name}({params})");
    if let Some(ret) = ret {
        sig.push_str(&format!(
            " : {}",
            &src[ast.expr_spans[ret.0 as usize].clone()]
        ));
    }
    sig.push_str(&effects_str(effects));
    if let FnKind::Impl { refines } = kind {
        sig.push_str(&format!(" refines {refines}"));
    }
    Some(sig)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drives `hover`/`goto_definition` directly against a one-document
    /// `docs` map — no stdio framing needed, since both take `&HashMap`/
    /// typed params rather than a live `Connection`. `line`/`col` are
    /// 0-indexed, matching LSP's own `Position`.
    fn doc(src: &str) -> HashMap<String, String> {
        let mut docs = HashMap::new();
        docs.insert("file:///t.tr".to_string(), src.to_string());
        docs
    }

    fn pos(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    fn hover_at(src: &str, line: u32, character: u32) -> Option<Hover> {
        let uri: Uri = "file:///t.tr".parse().unwrap();
        hover(
            &doc(src),
            HoverParams {
                text_document_position_params: lsp_types::TextDocumentPositionParams::new(
                    lsp_types::TextDocumentIdentifier::new(uri),
                    pos(line, character),
                ),
                work_done_progress_params: Default::default(),
            },
        )
    }

    fn goto_at(src: &str, line: u32, character: u32) -> Option<GotoDefinitionResponse> {
        let uri: Uri = "file:///t.tr".parse().unwrap();
        goto_definition(
            &doc(src),
            GotoDefinitionParams {
                text_document_position_params: lsp_types::TextDocumentPositionParams::new(
                    lsp_types::TextDocumentIdentifier::new(uri),
                    pos(line, character),
                ),
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            },
        )
    }

    fn hover_text(hover: &Hover) -> &str {
        match &hover.contents {
            HoverContents::Scalar(MarkedString::String(s)) => s,
            HoverContents::Markup(MarkupContent { value, .. }) => value,
            _ => panic!("unexpected hover contents shape: {:?}", hover.contents),
        }
    }

    const SRC: &str = "module M {\n    reg counter : [8] = 0\n    rule r {\n        counter := counter + 1\n    }\n}\n";

    #[test]
    fn hovering_a_use_site_still_works() {
        // The second `counter` on line 3 (`counter := counter + 1`, both
        // 0-indexed), well inside its `Expr::Ident` span (19..26) — the
        // pre-existing, already-covered case.
        let h = hover_at(SRC, 3, 22).expect("hover over a use site");
        let text = hover_text(&h);
        assert!(text.contains("counter"), "{text}");
        assert!(text.contains("[8]"), "{text}");
    }

    #[test]
    fn hovering_a_write_target_shows_its_type_too() {
        // The FIRST `counter` on line 3 — `counter := counter + 1`'s own
        // LHS, span 8..15, not the RHS read `hovering_a_use_site_still_
        // works` above deliberately targets instead. A write target is
        // typed through `type_write`, never `type_expr` (the only thing
        // that populates `expr_tys`), so this used to show just "`counter`
        // — a register" with no type at all — reproduced directly by an
        // `out` port write in `hovering_an_output_ports_write_site_shows_
        // its_type` below, but the gap was in `type_write`'s shared
        // `Expr::Ident` arm, not anything `out`-specific.
        let h = hover_at(SRC, 3, 10).expect("hover over the write target");
        let text = hover_text(&h);
        assert_eq!(text, "`counter: [8]` — a register");
    }

    const OUT_SRC: &str =
        "module M {\n    out v : [8] = 0\n    rule r {\n        v := 1\n    }\n}\n";

    #[test]
    fn hovering_an_output_ports_write_site_shows_its_type() {
        // `v := 1`, line 3 char 8 — `v`'s own write-target span.
        let h = hover_at(OUT_SRC, 3, 8).expect("hover over the output port's write site");
        assert_eq!(hover_text(&h), "`v: [8]` — an output port");
    }

    #[test]
    fn hovering_the_declaration_site_itself_now_resolves() {
        // `counter` in `reg counter : [8] = 0` itself — this is the gap
        // TODO.md flagged: not an `Expr::Ident`, so `ident_at` alone never
        // found it, and hover/goto-definition returned nothing here.
        let h = hover_at(SRC, 1, 8).expect("hover over the declaration site");
        let text = hover_text(&h);
        assert!(text.contains("counter"), "{text}");
        assert!(text.contains("[8]"), "{text}");
        assert!(text.contains("a register"), "{text}");
    }

    #[test]
    fn goto_definition_from_the_declaration_site_itself_now_resolves() {
        // Ctrl-clicking the declaration's own name is a legitimate
        // request too (many editors send it on any click, not just a
        // use) — it should resolve to itself rather than answering
        // nothing, matching `hovering_the_declaration_site_itself_now_
        // resolves` above.
        let resp = goto_at(SRC, 1, 8).expect("goto-definition over the declaration site");
        let GotoDefinitionResponse::Scalar(loc) = resp else {
            panic!("expected a single location, got {resp:?}");
        };
        assert_eq!(loc.range.start.line, 1);
    }

    #[test]
    fn hovering_a_declaration_site_with_no_scalar_type_shows_just_the_kind() {
        // A `rule`/`fn`/`module`/... declaration site has no entry in
        // `local_tys`/`state_tys` (nothing scalar to show) — same
        // graceful fallback an untyped use site already gets, just
        // reached through the declaration-site path instead.
        let src = "module M {\n    rule my_rule {\n    }\n}\n";
        let h = hover_at(src, 1, 9).expect("hover over the rule's own name");
        let text = hover_text(&h);
        assert_eq!(text, "`my_rule` — a rule");
    }

    #[test]
    fn hovering_a_keyword_finds_nothing() {
        // Line 1 char 4 is the `r` of the `reg` keyword itself — neither
        // an `Expr::Ident` use nor any def's own name span (that starts
        // at char 8, `counter`) — should resolve to nothing, not the
        // nearby declaration.
        assert!(hover_at(SRC, 1, 4).is_none());
    }

    // A plain `fn`-flavored function has no leading keyword in real
    // source syntax (`parser.rs` dispatches on a bare `Ident` at item
    // position) — matches `examples/call_nested_writes.tr`'s own
    // `Outer(x : [8]) : [8] <combines> { ... }`.
    const FN_SRC: &str = "module Top {\n    in a : [8]\n    out v_out : [8] = 0\n\n    \
                           Outer(x : [8]) : [8] <combines> {\n        v_out := x\n        \
                           return x\n    }\n\n    rule compute {\n        Outer(a)\n    }\n}\n";

    #[test]
    fn hovering_a_function_declaration_shows_its_signature_as_a_code_block() {
        let h = hover_at(FN_SRC, 4, 4).expect("hover over the fn's own declaration");
        assert_eq!(
            hover_text(&h),
            "```trace\nOuter(x : [8]) : [8] <combines>\n```\n\nA function."
        );
        let HoverContents::Markup(MarkupContent { kind, .. }) = h.contents else {
            panic!("expected markup content, got {:?}", h.contents);
        };
        assert_eq!(kind, MarkupKind::Markdown);
    }

    #[test]
    fn hovering_a_function_call_site_shows_the_same_signature() {
        // `Outer(a)` inside `rule compute` — a USE, not the declaration —
        // exercises the `Some(expr)` path through `thing_at` rather than
        // the declaration-site fallback the test above exercises.
        let h = hover_at(FN_SRC, 10, 8).expect("hover over a call site");
        assert_eq!(
            hover_text(&h),
            "```trace\nOuter(x : [8]) : [8] <combines>\n```\n\nA function."
        );
    }

    #[test]
    fn hovering_an_effect_keyword_shows_its_description_and_an_example() {
        // `combines` inside `Outer`'s own `<combines>` list, line 4 char
        // 29 — well inside the name's span (26..34), not the fn's own
        // name span (4..9), so this exercises `effect_hover`'s own
        // independent lookup path, not `thing_at`/`fn_signature` at all.
        let h = hover_at(FN_SRC, 4, 29).expect("hover over the combines effect");
        let text = hover_text(&h);
        assert!(text.starts_with("```trace\n<combines>\n```\n\n"), "{text}");
        assert!(text.contains("combinational"), "{text}");
        assert!(text.contains("```trace\nParity"), "{text}");
    }

    const EFFECT_ROW_SRC: &str = "module M {\n    in a : [8]\n    reg r : [8] = 0\n    \
                                   rule refill <reads {a}, writes {r}> {\n        r := a\n    \
                                   }\n}\n";

    #[test]
    fn hovering_reads_and_writes_gives_each_its_own_description() {
        // `reads {a}, writes {r}` on line 3: `reads` spans 17..22, `writes`
        // spans 28..34 — distinct keywords, distinct doc text (reads
        // mentions `in`, writes doesn't, since an `in` port can never be
        // written).
        let reads = hover_at(EFFECT_ROW_SRC, 3, 19).expect("hover over reads");
        let reads_text = hover_text(&reads);
        assert!(
            reads_text.starts_with("```trace\n<reads>\n```\n\n"),
            "{reads_text}"
        );
        assert!(reads_text.contains("`in`"), "{reads_text}");

        let writes = hover_at(EFFECT_ROW_SRC, 3, 30).expect("hover over writes");
        let writes_text = hover_text(&writes);
        assert!(
            writes_text.starts_with("```trace\n<writes>\n```\n\n"),
            "{writes_text}"
        );
        assert_ne!(reads_text, writes_text);
    }

    #[test]
    fn hovering_a_reads_writes_row_argument_resolves_like_a_real_state_reference() {
        // `a` inside `reads {a}` (char 24) and `r` inside `writes {r}`
        // (char 36) are effect ROW ARGUMENTS, not the effect keyword
        // itself — `effect_hover` only ever matches an `Effect::name`
        // span, never an arg `Name`, so these must NOT produce the
        // `reads`/`writes` doc (a looser "somewhere inside the effect"
        // check would have). Instead they resolve through `resolve::
        // Resolution::effect_arg_defs` to the actual `in`/`reg` they
        // name, same as any other state reference.
        let a = hover_at(EFFECT_ROW_SRC, 3, 24).expect("hover over the reads row's `a`");
        assert_eq!(hover_text(&a), "`a: [8]` — an input port");

        let r = hover_at(EFFECT_ROW_SRC, 3, 36).expect("hover over the writes row's `r`");
        assert_eq!(hover_text(&r), "`r: [8]` — a register");
    }

    #[test]
    fn goto_definition_from_a_reads_writes_row_argument_jumps_to_its_declaration() {
        let resp =
            goto_at(EFFECT_ROW_SRC, 3, 24).expect("goto-definition over the reads row's `a`");
        let GotoDefinitionResponse::Scalar(loc) = resp else {
            panic!("expected a single location, got {resp:?}");
        };
        assert_eq!(loc.range.start.line, 1); // `in a : [8]`

        let resp =
            goto_at(EFFECT_ROW_SRC, 3, 36).expect("goto-definition over the writes row's `r`");
        let GotoDefinitionResponse::Scalar(loc) = resp else {
            panic!("expected a single location, got {resp:?}");
        };
        assert_eq!(loc.range.start.line, 2); // `reg r : [8] = 0`
    }
}
