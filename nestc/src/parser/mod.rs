pub mod ast;
pub mod fmt;
pub mod lexer;
pub mod visitor;

pub mod parse;

// Grammar areas, each an `impl Parser`. They carry no public items of their own
// beyond the methods they add, so they need only be compiled in.
mod expr;
mod item;
mod pattern;
mod types;

pub mod pretty;

#[cfg(test)]
mod tests;
