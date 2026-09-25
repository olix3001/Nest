//! The Nest compiler, as a library: what the `nestc` binary drives, and what a
//! tool that needs the compiler's answers — the language server — calls.

#![feature(deref_patterns)]

pub mod codegen;
pub mod common;
pub mod driver;
pub mod ir;
pub mod library;
pub mod lir;
pub mod metadata;
pub mod parser;
pub mod sema;
