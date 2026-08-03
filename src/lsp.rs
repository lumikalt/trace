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

use crate::ast::{Ast, Expr, ExprId};
use crate::lexer::Span;
use crate::resolve::{DefId, Resolution};
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
    HoverContents, HoverParams, HoverProviderCapability, Location, MarkedString, OneOf, Position,
    PositionEncodingKind, PublishDiagnosticsParams, Range, ServerCapabilities,
    TextDocumentSyncCapability, TextDocumentSyncKind, Uri,
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

fn ident_def_at(ast: &Ast, res: &Resolution, offset: usize) -> Option<(ExprId, DefId)> {
    let expr = ident_at(ast, offset)?;
    let def = *res.expr_defs.get(&expr)?;
    Some((expr, def))
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
    let (_, def) = ident_def_at(ast, res, offset)?;
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
    let res = compiled.res.as_ref()?;
    let offset = index.offset(params.text_document_position_params.position);
    let (expr, def_id) = ident_def_at(ast, res, offset)?;
    let def = res.def(def_id);
    let text = match compiled.ty.as_ref().and_then(|ty| ty.expr_tys.get(&expr)) {
        Some(ty) => format!("`{}: {ty}` — {}", def.name, def.kind.describe()),
        None => format!("`{}` — {}", def.name, def.kind.describe()),
    };
    Some(Hover {
        contents: HoverContents::Scalar(MarkedString::String(text)),
        range: Some(index.range(&ast.expr_spans[expr.0 as usize])),
    })
}
