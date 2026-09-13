/*
 * The Nest runtime, in one file.
 *
 * Every symbol a generated object file refers to that is not a Nest function is
 * here, and there are six of them. That is the point: the language's runtime
 * surface is small enough to read, and swapping the collector is editing this
 * file and relinking rather than changing the compiler.
 *
 * ## Why there is a shim at all
 *
 * `new` and `make` allocate from the collector, and the generated code could
 * have called `GC_malloc` directly. It calls `nest_alloc` instead so that which
 * collector is linked is a **link-time** choice: the Boehm build and the
 * malloc-and-leak build produce the same object files from the same compiler.
 * Go and OCaml both put a shim here for the same reason. The indirection costs
 * nothing — with `NEST_GC_BOEHM` these are one-line forwards the C compiler
 * inlines.
 *
 * ## Which collector
 *
 * Boehm (bdwgc), when built with `-DNEST_GC_BOEHM`. It is **conservative**: it
 * finds roots by scanning the stack and registers itself, which is why the
 * backend emits no root maps even though `design/lir.md` §6 computed a precise
 * live set at every safepoint. Those live sets are what a precise or moving
 * collector would need, and this is the file that would change to want them.
 *
 * Without that define the allocator is `calloc` and nothing is ever collected,
 * which is a correct program that grows. It exists so the runtime builds with no
 * dependencies at all.
 *
 * ## Building
 *
 *   cc -c -O2 nest_runtime.c -o nest_runtime.o                    # leaking
 *   cc -c -O2 -DNEST_GC_BOEHM nest_runtime.c -o nest_runtime.o    # collected
 *
 * and then link a Nest object against it:
 *
 *   nestc --emit obj -o prog.o prog.nest
 *   cc prog.o nest_runtime.o -o prog            # add -lgc for the Boehm build
 *
 * `nestc` does this itself for an ordinary build: `nestc/build.rs` compiles this
 * file into `libnest_runtime.a` beside the compiler and `nestc prog.nest` links
 * against it, so the two commands above are what a *different* runtime is
 * substituted with — built how you like, and passed as `-C runtime=<path>`
 * (`-C link-arg=-lgc` for the Boehm build's dependency).
 */

#include <stdio.h>
#include <stdlib.h>

#ifdef NEST_GC_BOEHM
#include <gc.h>
#endif

/* Allocate `n` zeroed bytes that the collector owns.
 *
 * Zeroed because a Nest value is never observed before it is written, and a
 * collector scanning uninitialized bytes would see addresses that were never
 * pointers. `n == 0` still returns a distinct address: a zero-length slice has
 * a valid pointer (`design/lir.md` §10). */
void *nest_alloc(size_t n) {
    void *p;
#ifdef NEST_GC_BOEHM
    p = GC_malloc(n ? n : 1);
#else
    p = calloc(1, n ? n : 1);
#endif
    if (!p) {
        fputs("nest: out of memory\n", stderr);
        abort();
    }
    return p;
}

/* Release `p`.
 *
 * This is a **hint**, and under a collector it is one the collector may ignore.
 * It is the instruction the escape analysis emits on its own (§5) and the one a
 * written `drop(p)` produces, so there is one of them rather than two. */
void nest_free(void *p) {
    if (!p) {
        return;
    }
#ifdef NEST_GC_BOEHM
    GC_free(p);
#else
    free(p);
#endif
}

/* Run a collection now. `gc_collect()` in source. */
void nest_gc_collect(void) {
#ifdef NEST_GC_BOEHM
    GC_gcollect();
#endif
}

/* Prepare the collector. A program calls this before anything else.
 *
 * Boehm wants `GC_INIT()` on the main thread before the first allocation on
 * some platforms, and it is harmless everywhere else. Nothing calls this yet:
 * the compiler does not generate a C `main` wrapper, which is the piece the CLI
 * phase adds. */
void nest_init(void) {
#ifdef NEST_GC_BOEHM
    GC_INIT();
#endif
}

/* The last instruction: `trap()` in source, and where a panic ends up.
 *
 * It does not return, and the backend marks every call to it `noreturn`, which
 * is what makes the `unreachable` after it a guarantee rather than a claim. */
void nest_trap(void) {
    fputs("nest: trap\n", stderr);
    abort();
}

/* `assert(cond)`. The argument is a byte holding 0 or 1 — a Nest `bool` is a
 * byte in every slot, member and argument, which is what keeps its size the same
 * in a register and in memory. */
void nest_assert(unsigned char cond) {
    if (!cond) {
        fputs("nest: assertion failed\n", stderr);
        abort();
    }
}
