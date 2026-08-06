//! The staged compile pipeline, factored out of what used to be two
//! independently hand-maintained copies: main.rs's own CLI flow (which
//! never actually chained past `resolve`/`effects` before this existed —
//! `--firrtl` ran straight against the ORIGINAL source, so any example
//! using `<elaborates>`/closures could never reach real FIRRTL through
//! the CLI at all) and `tests/sim.rs`'s `generate_firrtl` (which DID
//! chain every stage, by hand, and was the only thing actually proving
//! the chain works). One shared implementation means the CLI path and
//! the tested path can never silently diverge — the specific failure
//! mode this exists to close off is `devenv.nix`'s `simulate` script
//! (three separate CLI invocations) quietly producing different output
//! from `cargo test` because one driver's stage list drifted from the
//! other's.
//!
//! Each stage is its own small function rather than one "compile
//! everything" call: main.rs needs to stop early at several points
//! (`--closures`/`--elaborate`/`--lower`/`--explain-schedule` each print
//! one intermediate stage's own output and exit), and a caller who wants
//! the whole way through just calls every stage in order — see
//! `tests/sim.rs`'s own use of these for that shape.
//!
//! Every stage's errors carry spans into whichever source string that
//! CALLER passed to it — never bundled with the source itself, since the
//! caller already has the right string in scope at each point (the same
//! shape main.rs's existing per-stage `report(&path, &src, ...)` calls
//! already use, just with more stages now).

use crate::ast::Ast;
use crate::bounds::Bounds;
use crate::effects::Effects;
use crate::lexer::Span;
use crate::resolve::Resolution;
use crate::schedule::Schedule;
use crate::types::Types;
use crate::{
    bounds, closures, effects, elaborate, firrtl, lexer, lower, parser, resolve, schedule, types,
};

#[derive(Debug, Clone)]
pub struct StageError {
    pub span: Span,
    pub message: String,
}

impl From<lexer::LexError> for StageError {
    fn from(e: lexer::LexError) -> Self {
        StageError {
            span: e.span,
            message: "unrecognized character(s)".to_string(),
        }
    }
}

macro_rules! stage_error_from {
    ($t:ty) => {
        impl From<$t> for StageError {
            fn from(e: $t) -> Self {
                StageError {
                    span: e.span,
                    message: e.message,
                }
            }
        }
    };
}
stage_error_from!(parser::ParseError);
stage_error_from!(resolve::ResolveError);
stage_error_from!(effects::EffectError);
stage_error_from!(types::TypeError);
stage_error_from!(elaborate::ElabError);
stage_error_from!(closures::ClosureError);
stage_error_from!(lower::LowerError);
stage_error_from!(bounds::BoundsError);
stage_error_from!(schedule::ScheduleError);
stage_error_from!(firrtl::EmitError);

fn collect<E: Into<StageError>>(errors: Vec<E>) -> Vec<StageError> {
    errors.into_iter().map(Into::into).collect()
}

pub struct Resolved {
    pub ast: Ast,
    pub res: Resolution,
}

/// lex -> parse -> resolve against `src`.
pub fn resolve_src(src: &str) -> Result<Resolved, Vec<StageError>> {
    let (tokens, lex_errors) = lexer::lex(src);
    if !lex_errors.is_empty() {
        return Err(collect(lex_errors));
    }
    let (ast, parse_errors) = parser::parse(src, &tokens);
    if !parse_errors.is_empty() {
        return Err(collect(parse_errors));
    }
    let (res, resolve_errors) = resolve::resolve(&ast);
    if !resolve_errors.is_empty() {
        return Err(collect(resolve_errors));
    }
    Ok(Resolved { ast, res })
}

pub struct Checked {
    pub ast: Ast,
    pub res: Resolution,
    pub fx: Effects,
    pub ty: Types,
}

/// `resolve_src`, then effects -> types, against `src`.
pub fn check(src: &str) -> Result<Checked, Vec<StageError>> {
    let Resolved { ast, res } = resolve_src(src)?;
    let (fx, effect_errors) = effects::check(&ast, &res);
    if !effect_errors.is_empty() {
        return Err(collect(effect_errors));
    }
    let (ty, type_errors) = types::check(&ast, &res, &fx);
    if !type_errors.is_empty() {
        return Err(collect(type_errors));
    }
    Ok(Checked { ast, res, fx, ty })
}

/// `closures::plan` + `elaborate::render` (fully generic span-
/// replacement, reused as-is — no reason for `closures.rs` to duplicate
/// it) against an already-`resolve_src`'d `r`/`src`. Returns the new,
/// closure-free source text; the caller re-`check`s it before the next
/// stage, same as every splice stage below.
pub fn splice_closures(r: &Resolved, src: &str) -> Result<String, Vec<StageError>> {
    let (edits, errors) = closures::plan(&r.ast, &r.res, src);
    if !errors.is_empty() {
        return Err(collect(errors));
    }
    Ok(elaborate::render(src, &edits))
}

/// `elaborate::plan` + `elaborate::render` against an already-`check`ed
/// `c`/`src`.
pub fn splice_elaborate(c: &Checked, src: &str) -> Result<String, Vec<StageError>> {
    let (edits, errors) = elaborate::plan(&c.ast, &c.res, &c.fx, src);
    if !errors.is_empty() {
        return Err(collect(errors));
    }
    Ok(elaborate::render(src, &edits))
}

/// `lower::plan` + `lower::render` against an already-`check`ed `c`/`src`.
pub fn splice_lower(c: &Checked, src: &str) -> Result<String, Vec<StageError>> {
    let (edits, errors) = lower::plan(&c.ast, &c.res, &c.fx, &c.ty);
    if !errors.is_empty() {
        return Err(collect(errors));
    }
    Ok(lower::render(&c.ast, src, &edits))
}

pub struct Scheduled {
    pub bounds: Bounds,
    pub sched: Schedule,
}

/// bounds -> schedule against an already-`check`ed `c` (the LAST stage's
/// own `Checked`, i.e. after every splice has already happened).
pub fn schedule_checked(c: &Checked) -> Result<Scheduled, Vec<StageError>> {
    let (bounds, bounds_errors) = bounds::check(&c.ast, &c.res, &c.fx, &c.ty);
    if !bounds_errors.is_empty() {
        return Err(collect(bounds_errors));
    }
    let (sched, schedule_errors) = schedule::schedule(&c.ast, &c.res, &c.fx, &c.ty, &bounds);
    if !schedule_errors.is_empty() {
        return Err(collect(schedule_errors));
    }
    Ok(Scheduled { bounds, sched })
}

/// FIRRTL emission against an already-`schedule_checked` `c`/`s`.
pub fn emit(c: &Checked, s: &Scheduled) -> Result<String, Vec<StageError>> {
    firrtl::emit(&c.ast, &c.res, &c.fx, &c.ty, &s.sched).map_err(collect)
}
