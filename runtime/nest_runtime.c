/*
 * The Nest runtime, in one file.
 *
 * Every symbol a generated object file refers to that is not a Nest function is
 * here, and there are eleven of them. That is the point: the language's runtime
 * surface is small enough to read, and swapping the collector is editing this
 * file and relinking rather than changing the compiler.
 *
 * ## Why there is a shim at all
 *
 * `new` and `make` allocate from the collector, and the generated code could
 * have called `GC_malloc` directly. It calls `nest_alloc` instead so that which
 * collector is linked is a **link-time** choice: another collector produces the
 * same object files from the same compiler.
 * Go and OCaml both put a shim here for the same reason. The indirection costs
 * nothing — these are one-line forwards the C compiler inlines.
 *
 * ## Which collector
 *
 * Boehm (bdwgc), and nothing else. It is **conservative**: it
 * finds roots by scanning the stack and registers itself, which is why the
 * backend emits no root maps even though `design/lir.md` §6 computed a precise
 * live set at every safepoint. Those live sets are what a precise or moving
 * collector would need, and this is the file that would change to want them.
 *
 * There used to be a second build, `calloc` with nothing ever collected. It was
 * removed: a program that grows until the machine runs out is not a correct
 * program, and a build that nothing tests is not one that keeps working.
 *
 * ## Building
 *
 *   cc -c -O2 -I<bdwgc>/include nest_runtime.c -o nest_runtime.o
 *
 * and then link a Nest object against it:
 *
 *   nestc --emit obj -o prog.o prog.nest
 *   cc prog.o nest_runtime.o <bdwgc>/lib/libgc.a -o prog
 *
 * `nestc` does this itself for an ordinary build: `nestc/build.rs` finds the
 * collector, compiles this file into `libnest_runtime.a` beside the compiler,
 * and `nestc prog.nest` links against both. The commands above are what a
 * *different* runtime is substituted with, passed as `-C runtime=<path>`.
 *
 * ## Poisoning frees
 *
 * With `NEST_GC_POISON` set in the environment, `nest_free` fills the object
 * with `0xDB` and keeps it instead of releasing it. A program that reads an
 * object after the compiler freed it then reads garbage every time, rather than
 * only when the collector happens to have handed the memory out again. It is
 * how the escape analysis is tested (`design/lir.md` §5).
 */

#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <sys/resource.h>
#include <string.h>

#include <gc.h>

/* Whether `NEST_GC_POISON` was set when the program started. */
static int poison;

/* Allocate `n` zeroed bytes that the collector owns.
 *
 * Zeroed because a Nest value is never observed before it is written, and a
 * collector scanning uninitialized bytes would see addresses that were never
 * pointers. `n == 0` still returns a distinct address: a zero-length slice has
 * a valid pointer (`design/lir.md` §10). */
/* How much stack the overflow handler is left to run in. One page is plenty
 * for a `fputs` and an `abort`, and a whole one keeps the margin aligned. */
#define NEST_STACK_MARGIN 65536

void *nest_alloc(size_t n) {
    void *p = GC_malloc(n ? n : 1);
    if (!p) {
        fputs("nest: out of memory\n", stderr);
        abort();
    }
    return p;
}

/* The objects `gc_leak` was given and `drop` has not released: a set of
 * addresses, open-addressed with linear probing, in a block the collector scans
 * and never collects. That block is what keeps each of them alive.
 *
 * A set rather than a list because `nest_free` asks it about every pointer it
 * frees, and most of those were never leaked. */
static void **leaked;
static size_t leaked_len, leaked_cap;

static size_t leaked_slot(void *p) {
    size_t h = (size_t)p >> 3;
    h ^= h >> 17;
    h *= 0x9E3779B97F4A7C15u;
    return (h ^ (h >> 29)) & (leaked_cap - 1);
}

static void leaked_insert(void *p) {
    size_t i = leaked_slot(p);
    while (leaked[i] && leaked[i] != p) {
        i = (i + 1) & (leaked_cap - 1);
    }
    if (!leaked[i]) {
        leaked[i] = p;
        leaked_len++;
    }
}

/* Keep `p` alive until it is freed. Leaking it twice is leaking it once. */
void nest_gc_leak(void *p) {
    if (!p) {
        return;
    }
    if ((leaked_len + 1) * 2 > leaked_cap) {
        void **old = leaked;
        size_t old_cap = leaked_cap;
        leaked_cap = old_cap ? old_cap * 2 : 64;
        leaked = GC_malloc_uncollectable(leaked_cap * sizeof *leaked);
        if (!leaked) {
            fputs("nest: out of memory\n", stderr);
            abort();
        }
        memset(leaked, 0, leaked_cap * sizeof *leaked);
        leaked_len = 0;
        for (size_t i = 0; i < old_cap; i++) {
            if (old[i]) {
                leaked_insert(old[i]);
            }
        }
        GC_free(old);
    }
    leaked_insert(p);
}

/* Forget `p` if it was leaked. Deleting from a linearly probed table shifts the
 * entries after it back, so a lookup never stops early at the hole. */
static void leaked_remove(void *p) {
    if (!leaked_len) {
        return;
    }
    size_t i = leaked_slot(p);
    while (leaked[i] != p) {
        if (!leaked[i]) {
            return;
        }
        i = (i + 1) & (leaked_cap - 1);
    }
    leaked[i] = NULL;
    leaked_len--;
    for (size_t j = (i + 1) & (leaked_cap - 1); leaked[j]; j = (j + 1) & (leaked_cap - 1)) {
        void *q = leaked[j];
        size_t home = leaked_slot(q);
        /* `q` may move into the hole at `i` unless its home lies cyclically
         * in (i, j], where it would no longer be found. */
        if ((j > i && (home <= i || home > j)) || (j < i && home <= i && home > j)) {
            leaked[i] = q;
            leaked[j] = NULL;
            i = j;
        }
    }
}

/* Release `p`.
 *
 * This is a **hint**, and under a collector it is one the collector may ignore.
 * It is the instruction the escape analysis emits on its own (§5) and the one a
 * written `drop(p)` produces, so there is one of them rather than two. It is
 * also what ends a `gc_leak`. */
void nest_free(void *p) {
    if (!p) {
        return;
    }
    leaked_remove(p);
    if (poison) {
        memset(p, 0xDB, GC_size(p));
        return;
    }
    GC_free(p);
}

/* Run a collection now. `gc_collect()` in source. */
void nest_gc_collect(void) {
    GC_gcollect();
}

/* The arguments the process was started with, kept.
 *
 * They are stored rather than fetched on demand because there is nowhere to
 * fetch them from: nothing in C or POSIX hands a running program its own
 * `argv` back. The one moment it exists is the call to `main`, so that is where
 * these are taken from.
 *
 * The pointers are the startup's own and are not copied: they outlive every
 * Nest value that borrows them, which is what makes a `str` cut out of `argv`
 * safe to hold. */
/* Prepare the collector.
 *
 * Boehm wants `GC_INIT()` on the main thread before the first allocation on
 * some platforms, and it is harmless everywhere else. The synthesized entry
 * point (`nestc/src/lir/entry.rs`) calls this first, before any of the
 * program's own code.
 *
 * It takes **nothing**. It used to be handed `argc` and `argv` and keep them,
 * which put a decision about what a Nest program does with its arguments in the
 * one file that is supposed to know only about machines. They go to whoever
 * claims `#lang("start")` now — `std/sys` — and this prepares the collector,
 * which is a fact about the machine and is all of what belongs here. */
/* The lowest stack address a Nest function may run at.
 *
 * Deep recursion is otherwise a SIGSEGV: the guard page below the stack is hit
 * by whatever instruction happens to touch it first, and the process dies with
 * no indication of which function ran away. A Nest program reports its own
 * failures — an index past the end, an overflow, a failed assertion — and this
 * is the same thing one level down.
 *
 * It is a plain global rather than a function because the generated code reads
 * it in the prologue of every function that can recurse: a load and a compare
 * that a branch predictor gets right every time. Zero means "not set", which
 * disables the check — a program whose limit could not be read keeps running
 * rather than refusing to start. */
uintptr_t nest_stack_floor = 0;

/* Where the stack ends, from the current frame and the limit the OS reports.
 *
 * The margin is what the handler itself needs: by the time the check fails
 * there must still be enough stack under it to print a line and abort. */
static void nest_stack_init(void) {
    struct rlimit rl;
    char here;
    uintptr_t sp = (uintptr_t)&here;
    if (getrlimit(RLIMIT_STACK, &rl) != 0) {
        return;
    }
    if (rl.rlim_cur == RLIM_INFINITY || rl.rlim_cur < NEST_STACK_MARGIN * 2) {
        return;
    }
    if ((uintptr_t)rl.rlim_cur >= sp) {
        return;
    }
    nest_stack_floor = sp - (uintptr_t)rl.rlim_cur + NEST_STACK_MARGIN;
}

/* The prologue check failed: this frame would run past the end of the stack. */
void nest_stack_overflow(void) {
    fputs("nest: stack overflow\n", stderr);
    abort();
}

void nest_init(void) {
    nest_stack_init();
    /* A sub-slice points into the middle of its array, and is all that may be
     * left of it. This is Boehm's default already; it is set here because the
     * language depends on it. */
    GC_set_all_interior_pointers(1);
    GC_INIT();
    poison = getenv("NEST_GC_POISON") != NULL;
}

/* The environment, `NAME=value` per entry and NULL-terminated.
 *
 * **`environ`, not `main`'s third parameter.** `main` does receive one, and it
 * is a snapshot: `setenv` may replace the table outright, and a program that
 * read the snapshot would then not see a variable it had just set — which is
 * how this came to be written this way rather than the other. `environ` is the
 * live table, and it is what `getenv` itself reads.
 *
 * POSIX declares it in `<unistd.h>`; it is declared here instead because macOS
 * hides that declaration behind a feature macro while still exporting the
 * symbol to an executable. A Nest program built as a **shared library** on
 * macOS would not find it, which is the one case this does not cover and the
 * one `std` does not claim to support yet.
 *
 * It may be NULL, which reads as an empty environment. */
extern char **environ;

char **nest_envp(void) {
    return environ;
}

/* The last error a libc call reported.
 *
 * `errno` is **a macro**, not a symbol — C says so, and on a threaded platform
 * it expands to a call returning a per-thread location (`__error()` on macOS,
 * `__errno_location()` on Linux). So there is nothing for `extern("c")` to
 * declare: a Nest declaration of `errno` would name a symbol that does not
 * exist on either platform, and naming one of the two expansions would be
 * naming the wrong one on the other.
 *
 * Reading it through C, where the macro is what it is, is the only portable
 * answer. `std/sys` calls this immediately after a failed call, which is what
 * `errno` requires of anyone reading it. */
int nest_errno(void) {
    return errno;
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
