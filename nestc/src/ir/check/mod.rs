//! IR validation: the checks that run on the lowered, linked program.
//!
//! These are the rules deliberately deferred out of inference. They share a
//! shape — walk the IR, report, change nothing — and they run *here* rather than
//! on the AST because by this point all surface sugar is resolved: an operator
//! is an explicit call carrying its [`Dispatch`](super::Dispatch) and a
//! `builtin` tag, a `for` is a loop, `.?` and `.!` are matches. A walk over the
//! IR therefore sees every operation that actually happens, and nothing hides
//! behind a piece of syntax the pass would have to learn to recognize. On the
//! AST each check would have to handle every surface form *and* re-derive which
//! operator resolved to which trait method, redoing work inference already did.
//!
//! They run on the whole-program [`Linked`] view, not per file: reachability
//! starts at one entry point, exhaustiveness needs every variant of an enum that
//! may be declared elsewhere, and a call crosses files freely.
//!
//! Every pass appends to one diagnostic list and none of them stop the others,
//! so a single run reports everything wrong with a program rather than the first
//! thing.

use crate::common::diagnostic::Diagnostic;
use crate::sema::def::DefTable;

use super::{Linked, Meta};

pub mod divergence;
pub mod mutability;

/// Run every IR validation pass over `linked`, in order, collecting what they
/// report.
pub fn run(defs: &DefTable, meta: &Meta, linked: &Linked) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    divergence::check(defs, meta, linked, &mut out);
    mutability::check(defs, meta, linked, &mut out);
    out
}
