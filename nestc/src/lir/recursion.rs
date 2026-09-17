//! Which functions can call themselves (§7d).
//!
//! The question has exactly one consumer: the stack check in a function's
//! prologue ([`FunctionAttrs::recursive`]). A function that cannot recurse runs
//! in a frame this compiler laid out and can bound, so no check it emitted could
//! ever fail. A function that can has no bound — the depth is the program's —
//! and running past the end of the stack is otherwise a SIGSEGV, which is the
//! one failure a Nest program does not report in its own words.
//!
//! "Can call itself" means **on the call graph**, not syntactically: mutual
//! recursion is the case that matters, and the pair of functions that call each
//! other names neither itself. That makes this a search for cycles, and the
//! answer is every function in a strongly connected component of more than one
//! member, plus every function with an edge to itself.
//!
//! A call **through a pointer** is not an edge here, because there is nothing to
//! draw an edge to: the callee is a value. Such a call is treated as possibly
//! recursive, and the function that makes one is marked — the alternative is
//! claiming a bound that the program can break by putting the function's own
//! address in a variable.

use super::{Callee, StmtKind, Unit};

/// Set [`FunctionAttrs::recursive`] on every function that can reach itself.
///
/// [`FunctionAttrs::recursive`]: super::FunctionAttrs::recursive
pub fn mark(program: &mut Unit) {
    // A `FuncId` **is** the position in `funcs` (§9), so the call graph needs
    // no index of its own.
    let mut edges: Vec<Vec<usize>> = vec![Vec::new(); program.funcs.len()];
    let mut opaque = vec![false; program.funcs.len()];
    for (i, f) in program.funcs.iter().enumerate() {
        for block in &f.blocks {
            for stmt in &block.stmts {
                let StmtKind::Call { callee, .. } = &stmt.kind else {
                    continue;
                };
                match callee {
                    Callee::Static(id) => {
                        let j = id.0 as usize;
                        if j < program.funcs.len() {
                            edges[i].push(j);
                        }
                    }
                    // The callee is a value, so there is nothing to draw an edge
                    // to and no bound to claim: a function that calls through a
                    // pointer might be calling itself.
                    Callee::Indirect(_) => opaque[i] = true,
                    _ => {}
                }
            }
        }
    }

    let mut recursive = opaque;
    for (i, e) in edges.iter().enumerate() {
        if e.contains(&i) {
            recursive[i] = true;
        }
    }
    for component in sccs(&edges) {
        if component.len() > 1 {
            for i in component {
                recursive[i] = true;
            }
        }
    }
    for (f, flag) in program.funcs.iter_mut().zip(recursive) {
        f.attrs.recursive |= flag;
    }
}

/// Tarjan's strongly connected components, iteratively.
///
/// Iteratively because the recursion this is looking for is the program's, and
/// a compiler that overflowed its own stack finding it would be a poor joke —
/// `core` and `std` linked together are thousands of functions deep in places.
fn sccs(edges: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let n = edges.len();
    let mut index = vec![usize::MAX; n];
    let mut low = vec![0usize; n];
    let mut on_stack = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut next = 0usize;
    let mut out = Vec::new();

    for root in 0..n {
        if index[root] != usize::MAX {
            continue;
        }
        // Each frame is a node and how far through its edges we are.
        let mut work: Vec<(usize, usize)> = vec![(root, 0)];
        while let Some((v, edge)) = work.pop() {
            if edge == 0 {
                index[v] = next;
                low[v] = next;
                next += 1;
                stack.push(v);
                on_stack[v] = true;
            }
            let mut descended = false;
            for (k, &w) in edges[v].iter().enumerate().skip(edge) {
                if index[w] == usize::MAX {
                    work.push((v, k + 1));
                    work.push((w, 0));
                    descended = true;
                    break;
                } else if on_stack[w] {
                    low[v] = low[v].min(index[w]);
                }
            }
            if descended {
                continue;
            }
            if low[v] == index[v] {
                let mut component = Vec::new();
                while let Some(w) = stack.pop() {
                    on_stack[w] = false;
                    component.push(w);
                    if w == v {
                        break;
                    }
                }
                out.push(component);
            }
            // Fold this node's result into its parent, which is the frame that
            // pushed it and is now on top.
            if let Some(&(parent, _)) = work.last() {
                low[parent] = low[parent].min(low[v]);
            }
        }
    }
    out
}
