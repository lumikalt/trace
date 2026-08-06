//! Type and width checking: DESIGN.md's two solvers, kept separate.
//!
//! Solver 1 (shapes + instantiation): every expression gets a `Ty`. Calls
//! to functions with implicit width parameters (`bits[N]`) instantiate
//! them by matching concrete argument widths — the unification half.
//!
//! Solver 2 (widths): widths are computed with monotone rules (`-` keeps
//! max width, `+` keeps max width AND GROWS BY ONE (the carry bit — no
//! silent wraparound, DESIGN.md's "Growing addition"), `*` sums,
//! comparisons give 1) and only *checked* where both sides are known.
//! Rebinding a local widens its width; bodies re-type until the local
//! table is stable, with a divergence cap.
//!
//! Generic bodies (widths depending on unsolved implicit params) are
//! shape-checked only; their widths check numerically at each concrete
//! call site after instantiation.
//!
//! Width rules follow Chisel-style modular arithmetic for every operator
//! EXCEPT `+` (`a - b`/`a & b`/etc. have width `max(|a|,|b|)`, no growth;
//! `a + b` has width `max(|a|,|b|)+1`, growing to hold the carry — the
//! ONE deliberate departure from Chisel's own default `+`), and an
//! integer literal absorbs into the other operand's width (it must fit).
//! Writing a wider value into a narrower register is an error that names
//! `trunc` — no silent truncation, `+`'s own carry bit included.

use crate::ast::{Ast, BinOp, ExprId, ItemId};
use crate::lexer::Span;
use crate::resolve::{DefId, DefKind, Resolution};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Width {
    Known(u64),
    /// Depends on an unsolved implicit parameter; checked at concrete
    /// call sites, not here.
    Unknown,
}

/// The `Width` a `Bits op Bits` expression combines to — `Mul` sums the
/// two; `Add` takes the max and grows by one (the carry bit: `n+1` bits
/// always, provably, holds the true sum of two `n`-bit unsigned values —
/// DESIGN.md's "Growing addition: no silent carry-bit truncation");
/// `Shl`/`Shr`/`AShr` keep the LEFT operand's own width (a shift amount
/// never widens/narrows the shifted value's own type); everything else
/// (`Sub`/`Div`/`Rem`/`BitAnd`/`BitOr`/`BitXor`) takes the max with no
/// growth — `Sub` deliberately NOT grown alongside `Add` despite the
/// obvious symmetry: unsigned underflow needs a genuinely different
/// mechanism than a width bit (there's no `n+1`-bit representation of
/// "less than zero" in an unsigned encoding), a separate, unstarted
/// design, not silently bundled in here; `Div`/`Rem` can never exceed
/// their dividend/divisor's own width so growing them would just force
/// pointless truncation elsewhere; `BitAnd`/`BitOr`/`BitXor` have no
/// carry by construction. `type_binop` (`types/expr.rs`) is this rule's
/// PRIMARY use, but it's a free function specifically so `firrtl`'s own
/// emission-time width resolver (`Emitter::resolve_bits_width`, for a
/// generic callee body's own expressions, whose static `expr_tys` entry
/// is deliberately `Unknown` per this module's own doc comment) can call
/// the SAME rule instead of an independently-maintained second copy —
/// the exact "const_fold`/`const_eval` drift" class of bug this codebase
/// has already found and fixed more than once. A comparison's own result
/// (`Ty::Bits(w)` is never this function's concern) never reaches here —
/// `type_binop` returns `l`'s own type for those, before this rule would
/// apply at all.
pub(crate) fn combine_bits_width(op: BinOp, a: Width, b: Width) -> Width {
    match op {
        BinOp::Mul => match (a, b) {
            (Width::Known(x), Width::Known(y)) => Width::Known(x + y),
            _ => Width::Unknown,
        },
        BinOp::Add => match (a, b) {
            (Width::Known(x), Width::Known(y)) => Width::Known(x.max(y) + 1),
            _ => Width::Unknown,
        },
        BinOp::Shl | BinOp::Shr | BinOp::AShr => a,
        _ => match (a, b) {
            (Width::Known(x), Width::Known(y)) => Width::Known(x.max(y)),
            _ => Width::Unknown,
        },
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ty {
    Bits(Width),
    Mem {
        elem: Box<Ty>,
        len: u64,
    },
    Fifo {
        elem: Box<Ty>,
        depth: u64,
    },
    /// A `spawn`'s result: `.result` yields the wrapped type, `.done`
    /// (bits[1]) reports whether the spawned FSM has finished.
    Handle(Box<Ty>),
    /// Elaboration-time integer (literals, `int` params).
    Int,
    /// `list[T]` — elaboration-time only, no runtime representation.
    /// Never carries a length: a `list[T]` param/body is type-checked
    /// generically (like a generic `bits[N]` body), the same length-
    /// agnostic shape check for any call site; the actual element COUNT
    /// only ever matters to the elaboration-time interpreter
    /// (`firrtl/elaborate.rs`), which operates on the real call site's
    /// `Expr::ListLit` directly, never through this type.
    List(Box<Ty>),
    Unit,
    /// A `struct Name { ... }` value. `name` is carried alongside `def`
    /// purely for `Display` (error messages read `Pair`, not a raw
    /// `DefId`) — `def` is what equality/lookup actually key off of;
    /// `struct_fields` (`Types`) holds the declared field list itself.
    Struct {
        def: DefId,
        name: String,
    },
    /// `?T` — sugar for a compiler-synthesized `{ valid: bit, data: T }`
    /// struct (see `firrtl`'s "Struct emission" for the flattening this
    /// shares with an ordinary declared struct). Unlike `Ty::Struct`,
    /// carries no `DefId`: there's no user declaration to key off of,
    /// `T` alone is the identity.
    Option(Box<Ty>),
    /// The literal `false`'s own type — unifies ONLY against a
    /// `Ty::Option` target (`check_assignable`), never a general
    /// `bits[1]`. Kept as its own sentinel (mirroring `Ty::Int`, the
    /// untyped-integer-literal placeholder) rather than reusing
    /// `Ty::Unknown`, which unifies with anything and would let `false`
    /// silently pass as a value of any type at all, not just an absent
    /// `?T`.
    AbsentLit,
    /// `optional <expr>`'s own type — mirrors `AbsentLit`: no standalone
    /// shape of its own, unifies ONLY against a `Ty::Option` target
    /// (`check_assignable`), which supplies the layer `optional` itself
    /// doesn't know. Carries the wrapped expression's `ExprId` (not a
    /// precomputed `Ty`) so unification recurses through `check_
    /// assignable` again on demand, against the TARGET's own inner —
    /// letting `optional (optional e)` peel one target layer per
    /// `optional` regardless of how many the target actually has.
    Optional(ExprId),
    /// Recovery type: unifies with anything, silences cascades.
    Unknown,
}

impl std::fmt::Display for Ty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Ty::Bits(Width::Known(w)) => write!(f, "[{w}]"),
            Ty::Bits(Width::Unknown) => write!(f, "[?]"),
            Ty::Mem { elem, len } => write!(f, "{elem}[{len}]"),
            Ty::Fifo { elem, depth } => write!(f, "fifo[{depth}] of {elem}"),
            Ty::Handle(inner) => write!(f, "handle of {inner}"),
            Ty::Int => write!(f, "int"),
            Ty::List(elem) => write!(f, "list[{elem}]"),
            Ty::Unit => write!(f, "unit"),
            Ty::Struct { name, .. } => write!(f, "struct {name}"),
            Ty::Option(inner) => write!(f, "?{inner}"),
            Ty::AbsentLit => write!(f, "false"),
            Ty::Optional(_) => write!(f, "optional"),
            Ty::Unknown => write!(f, "?"),
        }
    }
}

#[derive(Debug, Default)]
pub struct Types {
    pub expr_tys: HashMap<ExprId, Ty>,
    /// Type of every local (`let`/fresh `:=`) once its body's fixed point
    /// is reached. Keyed by `DefId` since locals are declared per rule/fn.
    pub local_tys: HashMap<DefId, Ty>,
    /// Declared type of every reg/mem/fifo, keyed by its `DefId`.
    pub state_tys: HashMap<DefId, Ty>,
    /// Every module's port list: `(port name, Input or Output, type)`.
    /// Keyed by the module's own `DefId`, not just modules that happen to
    /// be instantiated — `inst` may reference any top-level module.
    pub module_ports: HashMap<DefId, Vec<(String, DefKind, Ty)>>,
    /// An `inst` def -> the module `DefId` it instantiates. A separate
    /// side table rather than a `Ty` variant: an instance isn't a scalar
    /// value, only its ports are.
    pub instance_module: HashMap<DefId, DefId>,
    /// A `struct` def -> its declared, ordered field list. Keyed by the
    /// struct's own `DefId`, mirroring `module_ports`.
    pub struct_fields: HashMap<DefId, Vec<(String, Ty)>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeError {
    pub span: Span,
    pub message: String,
}

/// Re-type a body at most this many times waiting for local widths to
/// stabilize; hitting the cap means widths grow without bound.
const WIDEN_CAP: usize = 50;

/// The two synthetic field names every `?T` value has — shared between
/// the `.field`-read arm (`Ty::Option` case, expr.rs) and the
/// destructuring exhaustiveness check (`declared_fields`, collect.rs), so
/// a future change to `?T`'s shape only has one array to update.
const OPTION_FIELDS: [&str; 2] = ["valid", "data"];

// Split by responsibility, not by size: `collect` (the setup/collection
// pass that runs before per-body checking: state/struct/port collection,
// destructuring and attach validation, the per-body driver), `eval`
// (compile-time/const width evaluation), `stmt` (statement-level
// checking), `expr` (expression-level checking, solvers 1 and 2 proper).
// Each file's own doc comment says more.
mod collect;
mod eval;
mod expr;
mod stmt;

// Emission-time width resolution (`firrtl::Emitter::resolve_bits_width`)
// needs these two directly -- a nested generic call's own result width is
// found by re-running the identical `env`-building + width-expression-
// evaluation `type_call` does at type-check time, just with concretely-
// resolved argument widths instead of statically-known ones. Re-exported
// here (`eval` itself stays a private submodule) rather than duplicated.
pub(crate) use eval::{bits_width_expr, const_eval_expr, implicit_width_param};

pub fn check(ast: &Ast, res: &Resolution, fx: &crate::effects::Effects) -> (Types, Vec<TypeError>) {
    let mut checker = TypeChecker {
        ast,
        res,
        fx,
        def_items: res.item_defs.iter().map(|(i, d)| (*d, *i)).collect(),
        state_tys: HashMap::new(),
        types: Types::default(),
        errors: Vec::new(),
        emit: false,
    };
    // `collect_structs` first: `collect_state` now type-checks a struct-
    // typed reg/output's own init expression (missing/extra/mistyped
    // fields), which needs `struct_fields` already populated to validate
    // against — found the hard way when it wasn't (missing/extra-field
    // inits silently passed instead of erroring).
    checker.collect_structs();
    checker.collect_state();
    checker.collect_module_ports();
    checker.check_attaches();
    checker.check_all();
    // Runs last: needs every body's `expr_tys` already populated (see
    // `Destructure::source_field_base`'s own doc comment, ast.rs), and
    // runs exactly once (unlike a rule/fn body's own fixpoint re-typing,
    // there's nothing here to re-stabilize), so there's no risk of the
    // widening loop's usual double-emit problem.
    checker.check_destructures();
    (checker.types, checker.errors)
}

struct TypeChecker<'a> {
    ast: &'a Ast,
    res: &'a Resolution,
    /// Computed effect signatures (`fails`, in particular) -- needed only
    /// by `Stmt::IfLet`'s own "is this a failing call?" check, see `is_
    /// failing_call` (expr.rs); nothing else here reads it.
    fx: &'a crate::effects::Effects,
    def_items: HashMap<DefId, ItemId>,
    /// Declared type of every reg/mem/fifo def.
    state_tys: HashMap<DefId, Ty>,
    types: Types,
    errors: Vec<TypeError>,
    /// Errors are emitted only on the final, stable typing pass.
    emit: bool,
}

fn bits_needed(v: u64) -> u64 {
    (64 - v.leading_zeros() as u64).max(1)
}

fn clog2(v: u64) -> u64 {
    if v <= 1 {
        0
    } else {
        64 - (v - 1).leading_zeros() as u64
    }
}

/// `prio`'s own result width, given its argument's -- the single source
/// of truth `type_builtin_call`'s own `"prio"` arm and `firrtl`'s
/// emission-time `Emitter::resolve_bits_width` (a generic callee body's
/// `let g = prio(reqs); ...`, where `reqs`'s own width is only known
/// once substituted at a concrete call site) both call, rather than each
/// keeping its own copy of the `clog2(w).max(1)` rule.
pub(crate) fn prio_result_width(arg_width: u64) -> u64 {
    clog2(arg_width).max(1)
}

/// `popcount`'s own result width, given its argument's — the single
/// source of truth `type_builtin_call`'s `"popcount"` arm and firrtl's
/// emission-time `resolve_bits_width` both call, same reasoning as
/// `prio_result_width` above. A `w`-bit argument's set-bit count ranges
/// `0..=w`, `w+1` distinct values, so `clog2(w + 1)` bits — NOT
/// `clog2(w)`, which only covers `0..w` and would silently truncate the
/// all-ones case.
pub(crate) fn popcount_result_width(arg_width: u64) -> u64 {
    clog2(arg_width + 1).max(1)
}

impl<'a> TypeChecker<'a> {
    pub(crate) fn error(&mut self, span: Span, message: String) {
        if self.emit {
            self.errors.push(TypeError { span, message });
        }
    }

    pub(crate) fn expr_span(&self, id: ExprId) -> Span {
        self.ast.expr_spans[id.0 as usize].clone()
    }
}
