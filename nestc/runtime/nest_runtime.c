/*
 * The Nest runtime, in one file.
 *
 * Every symbol a generated object file refers to that is not a Nest function is
 * here, and there are fourteen of them. That is the point: the language's runtime
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
#include <setjmp.h>
#include <unistd.h>
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

/* Bytes to stderr, and nothing else.
 *
 * This is the whole of what the runtime does for a failing program. `core` has
 * no I/O of its own — it does not know whether a target has a console — so it
 * needs *a* way to emit bytes; it does not need the runtime to decide what a
 * panic looks like. The shape of the report ("nest: panic: ", the `at
 * file:line:column` line) is assembled in `core/fail.nest`, where it can be
 * changed without touching C.
 *
 * The text is a pointer and a length because that is what a Nest `str` is
 * (`design/lir.md` §7b) and it is not NUL terminated. A zero length is normal
 * and reads nothing. */
void nest_write_err(const unsigned char *buf, size_t len) {
    if (buf != NULL && len > 0) {
        fwrite(buf, 1, len, stderr);
    }
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
 * is what makes the `unreachable` after it a guarantee rather than a claim.
 *
 * It prints **nothing**. A trap is the machine instruction at the end of a
 * failure, not the failure's report: everything that has something to say —
 * a panic the program wrote, a trapped overflow, an index past the end — says
 * it through `core`'s panic handler first and only then arrives here. A
 * `trap()` called directly is a program asking to stop without a word, and
 * that is what it gets. */
void nest_trap(void) {
    abort();
}

/* Whether what is written to stderr should carry colour.
 *
 * A terminal on the other end, and `NO_COLOR` unset: the two halves of the
 * convention every tool follows. Both are C's to answer — `isatty` is a
 * syscall and the environment is the process's — which is why the decision is
 * here and only the *escapes* are in Nest. */
int nest_color_stderr(void) {
    const char *no = getenv("NO_COLOR");
    if (no != NULL && no[0] != '\0') {
        return 0;
    }
    return isatty(2) ? 1 : 0;
}

/* ===< Guarding one call against a panic >===
 *
 * What a test binary needs and nothing else does: a way to call a function and
 * come back when it *failed*, so that one failing test does not end the run.
 *
 * A panic does not unwind (`design/lir.md`), so there is no stack to walk back
 * and no landing pad to arrive at — which leaves `setjmp`/`longjmp`, and leaves
 * it here, because `setjmp` only works in the frame that called it. A wrapper
 * in Nest would return before the jump could be taken.
 *
 * `core`'s panic handler calls `nest_guard_fail` after it has printed its
 * report and before it traps. With nothing guarding, that call returns and the
 * trap happens as it always did: an ordinary program is unchanged.
 *
 * Nothing between the two runs: a `defer` the failing call was holding does not
 * run, because there is no unwinding to run it. A test binary exits after its
 * suite, which is why that is affordable here and would not be in general. */
static jmp_buf nest_guard_buf;
static int nest_guard_armed = 0;

/* Call `fn`, returning 0 if it returned and 1 if it panicked.
 *
 * Not reentrant: one buffer, so a guarded call inside a guarded call would
 * return to the outer one. The runner below it makes no such call. */
int nest_guard_run(void (*fn)(void)) {
    if (setjmp(nest_guard_buf) != 0) {
        nest_guard_armed = 0;
        return 1;
    }
    nest_guard_armed = 1;
    fn();
    nest_guard_armed = 0;
    return 0;
}

/* Leave the guarded call, if there is one. Returns when there is not. */
void nest_guard_fail(void) {
    if (nest_guard_armed) {
        nest_guard_armed = 0;
        longjmp(nest_guard_buf, 1);
    }
}

/* ===< f128 >===
 *
 * IEEE-754 binary128 in software, on every target.
 *
 * LLVM lowers `fp128` arithmetic to libcalls (`__addtf3`, `__lttf2`, ...), and
 * whether those exist is the platform's business: glibc's libgcc has them, and
 * macOS on arm64 does not — its `long double` is a `double`, so nothing there
 * ever needed one. And they cannot be written here in C for such a target,
 * because they take their arguments in float registers and C has no type that
 * is passed like an `fp128` on it.
 *
 * So the compiler does not emit them. Every `f128` operation is lowered to a
 * call to one of the functions below, handing the value's **bits** as an
 * `i128`, which every target passes the same way C's `unsigned __int128` is.
 * The format: 1 sign bit, 15 exponent bits (bias 16383), 112 fraction bits.
 *
 * Rounding is to nearest, ties to even, throughout. A NaN that comes out is the
 * quiet NaN; one that goes in comes back quieted. */

typedef unsigned __int128 nest_u128;
typedef __int128 nest_i128;

#define Q_SIG_BITS 112
#define Q_EXP_MAX 0x7fff
#define Q_BIAS 16383
#define Q_IMPLICIT ((nest_u128)1 << Q_SIG_BITS)
#define Q_SIG_MASK (Q_IMPLICIT - 1)
#define Q_SIGN ((nest_u128)1 << 127)
#define Q_ABS_MASK (Q_SIGN - 1)
#define Q_INF ((nest_u128)Q_EXP_MAX << Q_SIG_BITS)
#define Q_QUIET ((nest_u128)1 << (Q_SIG_BITS - 1))
#define Q_NAN (Q_INF | Q_QUIET)

static int q_clz(nest_u128 x) {
    uint64_t hi = (uint64_t)(x >> 64);
    if (hi != 0) {
        return __builtin_clzll(hi);
    }
    uint64_t lo = (uint64_t)x;
    return lo == 0 ? 128 : 64 + __builtin_clzll(lo);
}

/* `x >> n`, with every bit shifted out folded into the lowest bit — the
 * sticky bit rounding reads. */
static nest_u128 q_shr_sticky(nest_u128 x, int n) {
    if (n <= 0) {
        return x;
    }
    if (n >= 128) {
        return x != 0;
    }
    return (x >> n) | ((x << (128 - n)) != 0);
}

/* A subnormal's significand, shifted up until its leading bit is where the
 * implicit bit of a normal one is; answers the exponent that makes up for it. */
static int q_normalize(nest_u128 *sig) {
    int shift = q_clz(*sig) - q_clz(Q_IMPLICIT);
    *sig <<= shift;
    return 1 - shift;
}

/* Round and pack. `sig` carries the implicit bit at bit 115 and three more
 * below the fraction — guard, round, sticky — and `exp` is the biased exponent
 * it would have as a normal number. */
static nest_u128 q_round_pack(nest_u128 sign, int exp, nest_u128 sig) {
    if (exp >= Q_EXP_MAX) {
        return sign | Q_INF;
    }
    if (exp <= 0) {
        sig = q_shr_sticky(sig, 1 - exp);
        exp = 0;
    }
    int grs = (int)(sig & 7);
    nest_u128 r = ((sig >> 3) & Q_SIG_MASK) | ((nest_u128)exp << Q_SIG_BITS) | sign;
    /* A carry out of the fraction lands in the exponent, which is right: a
     * subnormal becomes the least normal, and the greatest finite value
     * becomes infinity. */
    if (grs > 4 || (grs == 4 && (r & 1))) {
        r += 1;
    }
    return r;
}

/* Split a finite, nonzero value into its biased exponent and a significand
 * with the implicit bit at bit 112. */
static int q_unpack(nest_u128 x, nest_u128 *sig) {
    int exp = (int)((x >> Q_SIG_BITS) & Q_EXP_MAX);
    *sig = x & Q_SIG_MASK;
    if (exp == 0) {
        return q_normalize(sig);
    }
    *sig |= Q_IMPLICIT;
    return exp;
}

static nest_u128 q_add(nest_u128 a, nest_u128 b) {
    nest_u128 a_abs = a & Q_ABS_MASK;
    nest_u128 b_abs = b & Q_ABS_MASK;
    if (a_abs > Q_INF || b_abs > Q_INF) {
        return Q_NAN;
    }
    if (a_abs == Q_INF) {
        return (b_abs == Q_INF && ((a ^ b) & Q_SIGN)) ? Q_NAN : a;
    }
    if (b_abs == Q_INF) {
        return b;
    }
    if (a_abs == 0) {
        return b_abs == 0 ? (a & b) : b;
    }
    if (b_abs == 0) {
        return a;
    }
    if (b_abs > a_abs) {
        nest_u128 t = a;
        a = b;
        b = t;
    }
    nest_u128 a_sig, b_sig;
    int a_exp = q_unpack(a, &a_sig);
    int b_exp = q_unpack(b, &b_sig);
    nest_u128 sign = a & Q_SIGN;
    a_sig <<= 3;
    b_sig = q_shr_sticky(b_sig << 3, a_exp - b_exp);
    if ((a ^ b) & Q_SIGN) {
        a_sig -= b_sig;
        if (a_sig == 0) {
            return 0;
        }
        int shift = q_clz(a_sig) - q_clz(Q_IMPLICIT << 3);
        if (shift > 0) {
            a_sig <<= shift;
            a_exp -= shift;
        }
    } else {
        a_sig += b_sig;
        if (a_sig & (Q_IMPLICIT << 4)) {
            a_sig = q_shr_sticky(a_sig, 1);
            a_exp += 1;
        }
    }
    return q_round_pack(sign, a_exp, a_sig);
}

static nest_u128 q_mul(nest_u128 a, nest_u128 b) {
    nest_u128 sign = (a ^ b) & Q_SIGN;
    nest_u128 a_abs = a & Q_ABS_MASK;
    nest_u128 b_abs = b & Q_ABS_MASK;
    if (a_abs > Q_INF || b_abs > Q_INF) {
        return Q_NAN;
    }
    if (a_abs == Q_INF || b_abs == Q_INF) {
        return (a_abs == 0 || b_abs == 0) ? Q_NAN : (sign | Q_INF);
    }
    if (a_abs == 0 || b_abs == 0) {
        return sign;
    }
    nest_u128 a_sig, b_sig;
    int exp = q_unpack(a, &a_sig) + q_unpack(b, &b_sig) - Q_BIAS;
    /* The 226-bit product, as `hi:lo`. */
    uint64_t a0 = (uint64_t)a_sig, a1 = (uint64_t)(a_sig >> 64);
    uint64_t b0 = (uint64_t)b_sig, b1 = (uint64_t)(b_sig >> 64);
    nest_u128 p00 = (nest_u128)a0 * b0, p01 = (nest_u128)a0 * b1;
    nest_u128 p10 = (nest_u128)a1 * b0, p11 = (nest_u128)a1 * b1;
    nest_u128 mid = (p00 >> 64) + (uint64_t)p01 + (uint64_t)p10;
    nest_u128 lo = (mid << 64) | (uint64_t)p00;
    nest_u128 hi = p11 + (p01 >> 64) + (p10 >> 64) + (mid >> 64);
    /* The leading bit is 224 or 225; bring it to 115, folding the rest in. */
    int top = (hi >> (225 - 128)) & 1 ? 225 : 224;
    exp += top - 224;
    int shift = top - 115;
    nest_u128 sticky = (lo << (128 - shift)) != 0;
    nest_u128 sig = (hi << (128 - shift)) | (lo >> shift) | sticky;
    return q_round_pack(sign, exp, sig);
}

static nest_u128 q_div(nest_u128 a, nest_u128 b) {
    nest_u128 sign = (a ^ b) & Q_SIGN;
    nest_u128 a_abs = a & Q_ABS_MASK;
    nest_u128 b_abs = b & Q_ABS_MASK;
    if (a_abs > Q_INF || b_abs > Q_INF) {
        return Q_NAN;
    }
    if (a_abs == Q_INF) {
        return b_abs == Q_INF ? Q_NAN : (sign | Q_INF);
    }
    if (b_abs == Q_INF) {
        return sign;
    }
    if (b_abs == 0) {
        return a_abs == 0 ? Q_NAN : (sign | Q_INF);
    }
    if (a_abs == 0) {
        return sign;
    }
    nest_u128 a_sig, b_sig;
    int exp = q_unpack(a, &a_sig) - q_unpack(b, &b_sig) + Q_BIAS;
    if (a_sig < b_sig) {
        a_sig <<= 1;
        exp -= 1;
    }
    /* Long division, one quotient bit at a time: 113 bits and two more. */
    nest_u128 rem = a_sig;
    nest_u128 quo = 0;
    for (int i = 0; i < 115; i++) {
        quo <<= 1;
        if (rem >= b_sig) {
            rem -= b_sig;
            quo |= 1;
        }
        rem <<= 1;
    }
    return q_round_pack(sign, exp, (quo << 1) | (rem != 0));
}

/* `fmod`: the remainder whose sign is the dividend's, which is exact. */
static nest_u128 q_rem(nest_u128 a, nest_u128 b) {
    nest_u128 a_abs = a & Q_ABS_MASK;
    nest_u128 b_abs = b & Q_ABS_MASK;
    if (a_abs >= Q_INF || b_abs > Q_INF || b_abs == 0) {
        return Q_NAN;
    }
    if (b_abs == Q_INF || a_abs < b_abs) {
        return a;
    }
    nest_u128 sign = a & Q_SIGN;
    nest_u128 r, d;
    int a_exp = q_unpack(a, &r);
    int b_exp = q_unpack(b, &d);
    for (int e = a_exp; e > b_exp; e--) {
        if (r >= d) {
            r -= d;
        }
        r <<= 1;
    }
    if (r >= d) {
        r -= d;
    }
    if (r == 0) {
        return sign;
    }
    int exp = b_exp;
    while (r < Q_IMPLICIT) {
        r <<= 1;
        exp -= 1;
    }
    if (exp <= 0) {
        r >>= 1 - exp;
        exp = 0;
    }
    return sign | ((nest_u128)exp << Q_SIG_BITS) | (r & Q_SIG_MASK);
}

/* Order `a` against `b`: -1, 0 or 1, and 2 when either is a NaN. */
static int q_cmp(nest_u128 a, nest_u128 b) {
    nest_u128 a_abs = a & Q_ABS_MASK;
    nest_u128 b_abs = b & Q_ABS_MASK;
    if (a_abs > Q_INF || b_abs > Q_INF) {
        return 2;
    }
    if ((a_abs | b_abs) == 0) {
        return 0;
    }
    nest_i128 ai = (nest_i128)a;
    nest_i128 bi = (nest_i128)b;
    /* Sign and magnitude: as integers, the positives order themselves, and the
     * negatives order backwards. */
    if ((ai & bi) >= 0) {
        return ai < bi ? -1 : ai == bi ? 0 : 1;
    }
    return ai > bi ? -1 : ai == bi ? 0 : 1;
}

/* A `double` (so also an `f32` or an `f16`, which widen to one exactly),
 * widened. Exact. */
static nest_u128 q_from_f64(double v) {
    uint64_t x;
    memcpy(&x, &v, sizeof x);
    nest_u128 sign = (nest_u128)(x >> 63) << 127;
    int exp = (int)((x >> 52) & 0x7ff);
    nest_u128 frac = x & ((1ULL << 52) - 1);
    if (exp == 0x7ff) {
        return sign | (frac ? Q_NAN : Q_INF);
    }
    if (exp == 0) {
        if (frac == 0) {
            return sign;
        }
        int shift = q_clz(frac) - q_clz((nest_u128)1 << 52);
        frac = (frac << shift) & ((1ULL << 52) - 1);
        exp = 1 - shift;
    }
    return sign | ((nest_u128)(exp - 1023 + Q_BIAS) << Q_SIG_BITS) | (frac << (Q_SIG_BITS - 52));
}

/* Narrowed to the binary format of `e` exponent and `m` fraction bits,
 * rounding once: the bits of an `f64`, `f32` or `f16`. */
static uint64_t q_narrow(nest_u128 x, int e, int m) {
    uint64_t sign = (uint64_t)(x >> 127) << (e + m);
    int dst_max = (1 << e) - 1;
    int exp = (int)((x >> Q_SIG_BITS) & Q_EXP_MAX);
    nest_u128 frac = x & Q_SIG_MASK;
    if (exp == Q_EXP_MAX) {
        uint64_t nan = frac ? ((uint64_t)1 << (m - 1)) : 0;
        return sign | ((uint64_t)dst_max << m) | nan;
    }
    if (exp == 0 && frac == 0) {
        return sign;
    }
    int dst_exp = exp - Q_BIAS + (dst_max >> 1);
    nest_u128 sig = frac | (exp ? Q_IMPLICIT : 0);
    if (dst_exp >= dst_max) {
        return sign | ((uint64_t)dst_max << m);
    }
    int shift = Q_SIG_BITS - m - 3;
    if (dst_exp <= 0) {
        shift += 1 - dst_exp;
        dst_exp = 0;
    }
    nest_u128 s3 = q_shr_sticky(sig, shift);
    int grs = (int)(s3 & 7);
    uint64_t r = (uint64_t)((s3 >> 3) & (((nest_u128)1 << m) - 1));
    r |= (uint64_t)dst_exp << m;
    if (grs > 4 || (grs == 4 && (r & 1))) {
        r += 1;
    }
    return sign | r;
}

static nest_u128 q_from_u128(nest_u128 v, nest_u128 sign) {
    if (v == 0) {
        return sign;
    }
    int msb = 127 - q_clz(v);
    nest_u128 sig = msb > 115 ? q_shr_sticky(v, msb - 115) : v << (115 - msb);
    return q_round_pack(sign, msb + Q_BIAS, sig);
}

/* Toward zero, saturating at the ends of `u128`/`i128`; a NaN is zero. */
static nest_u128 q_to_magnitude(nest_u128 x, int limit, int *over) {
    int exp = (int)((x >> Q_SIG_BITS) & Q_EXP_MAX);
    *over = 0;
    if (exp == Q_EXP_MAX && (x & Q_SIG_MASK)) {
        return 0;
    }
    if (exp < Q_BIAS) {
        return 0;
    }
    int e = exp - Q_BIAS;
    if (e >= limit) {
        *over = 1;
        return 0;
    }
    nest_u128 sig = (x & Q_SIG_MASK) | Q_IMPLICIT;
    return e >= Q_SIG_BITS ? sig << (e - Q_SIG_BITS) : sig >> (Q_SIG_BITS - e);
}

nest_u128 nest_f128_add(nest_u128 a, nest_u128 b) { return q_add(a, b); }
nest_u128 nest_f128_sub(nest_u128 a, nest_u128 b) { return q_add(a, b ^ Q_SIGN); }
nest_u128 nest_f128_mul(nest_u128 a, nest_u128 b) { return q_mul(a, b); }
nest_u128 nest_f128_div(nest_u128 a, nest_u128 b) { return q_div(a, b); }
nest_u128 nest_f128_rem(nest_u128 a, nest_u128 b) { return q_rem(a, b); }
int32_t nest_f128_cmp(nest_u128 a, nest_u128 b) { return q_cmp(a, b); }

nest_u128 nest_f128_from_f64(double v) { return q_from_f64(v); }
double nest_f128_to_f64(nest_u128 x) {
    uint64_t bits = q_narrow(x, 11, 52);
    double d;
    memcpy(&d, &bits, sizeof d);
    return d;
}
uint32_t nest_f128_to_f32_bits(nest_u128 x) { return (uint32_t)q_narrow(x, 8, 23); }
uint16_t nest_f128_to_f16_bits(nest_u128 x) { return (uint16_t)q_narrow(x, 5, 10); }

nest_u128 nest_f128_from_u128(nest_u128 v) { return q_from_u128(v, 0); }
nest_u128 nest_f128_from_i128(nest_i128 v) {
    return v < 0 ? q_from_u128((nest_u128)0 - (nest_u128)v, Q_SIGN) : q_from_u128((nest_u128)v, 0);
}
nest_u128 nest_f128_to_u128(nest_u128 x) {
    if (x & Q_SIGN) {
        return 0;
    }
    int over;
    nest_u128 m = q_to_magnitude(x, 128, &over);
    return over ? ~(nest_u128)0 : m;
}
nest_i128 nest_f128_to_i128(nest_u128 x) {
    int over;
    nest_u128 m = q_to_magnitude(x, 127, &over);
    nest_u128 max = ((nest_u128)1 << 127) - 1;
    if (x & Q_SIGN) {
        /* -2^127 is the one magnitude past `max` that still fits. */
        return over ? (nest_i128)((nest_u128)1 << 127) : -(nest_i128)m;
    }
    return over ? (nest_i128)max : (nest_i128)m;
}

/* ===< Writing an f128 in decimal >===
 *
 * `f64`'s `Display` asks `snprintf` for its digits; nothing in C writes a
 * binary128, so its digits are found here, exactly, with Steele & White's
 * digit generation over big integers (Dragon4, in Burger & Dybvig's form): the
 * value is `r / s`, each digit is how many `s` fit in `10 r`, and the shortest
 * text is the first one that lies within half an ulp of the value on either
 * side. The layout — positional between `1e-5` and `1e17`, `1.5e7` outside
 * them, `NaN`, `inf` — is `core/fmt.nest`'s, so an `f128` prints the way every
 * other float does. */

/* Enough for 2^16384 times 10^4966, the largest either side ever gets.
 *
 * The working numbers are `static`: a few kilobytes each is more than a stack
 * frame should hold, and a Nest program formats on one thread. */
#define BIG_WORDS 1200

typedef struct {
    uint32_t w[BIG_WORDS];
    int n; /* words in use; w[n-1] != 0 unless n == 0 */
} Big;

static void big_trim(Big *a) {
    while (a->n > 0 && a->w[a->n - 1] == 0) {
        a->n--;
    }
}

static void big_set(Big *a, nest_u128 v) {
    a->n = 0;
    while (v != 0) {
        a->w[a->n++] = (uint32_t)v;
        v >>= 32;
    }
}

static void big_shl(Big *a, int bits) {
    if (a->n == 0 || bits == 0) {
        return;
    }
    int words = bits / 32;
    int rest = bits % 32;
    int n = a->n + words + 1;
    for (int i = n - 1; i >= 0; i--) {
        int src = i - words;
        uint64_t hi = (src >= 0 && src < a->n) ? a->w[src] : 0;
        uint64_t lo = (src - 1 >= 0 && src - 1 < a->n) ? a->w[src - 1] : 0;
        a->w[i] = rest ? (uint32_t)((hi << rest) | (lo >> (32 - rest))) : (uint32_t)hi;
    }
    a->n = n;
    big_trim(a);
}

static void big_mul_small(Big *a, uint32_t m) {
    uint64_t carry = 0;
    for (int i = 0; i < a->n; i++) {
        uint64_t p = (uint64_t)a->w[i] * m + carry;
        a->w[i] = (uint32_t)p;
        carry = p >> 32;
    }
    if (carry) {
        a->w[a->n++] = (uint32_t)carry;
    }
}

static void big_pow10(Big *a, int k) {
    for (; k >= 9; k -= 9) {
        big_mul_small(a, 1000000000u);
    }
    static const uint32_t small[] = {1, 10, 100, 1000, 10000, 100000, 1000000, 10000000, 100000000};
    big_mul_small(a, small[k]);
}

static int big_cmp(const Big *a, const Big *b) {
    if (a->n != b->n) {
        return a->n < b->n ? -1 : 1;
    }
    for (int i = a->n - 1; i >= 0; i--) {
        if (a->w[i] != b->w[i]) {
            return a->w[i] < b->w[i] ? -1 : 1;
        }
    }
    return 0;
}

/* `a + b` compared against `c`, without building the sum anywhere. */
static int big_cmp_sum(const Big *a, const Big *b, const Big *c) {
    static Big t;
    uint64_t carry = 0;
    int n = a->n > b->n ? a->n : b->n;
    for (int i = 0; i < n; i++) {
        uint64_t s = carry + (i < a->n ? a->w[i] : 0) + (i < b->n ? b->w[i] : 0);
        t.w[i] = (uint32_t)s;
        carry = s >> 32;
    }
    t.n = n;
    if (carry) {
        t.w[t.n++] = (uint32_t)carry;
    }
    return big_cmp(&t, c);
}

static void big_sub(Big *a, const Big *b) {
    int64_t borrow = 0;
    for (int i = 0; i < a->n; i++) {
        int64_t d = (int64_t)a->w[i] - (i < b->n ? b->w[i] : 0) - borrow;
        borrow = d < 0;
        a->w[i] = (uint32_t)(d + (borrow << 32));
    }
    big_trim(a);
}

/* How many `s` fit in `r` — never ten, by construction — leaving `r` the rest. */
static int big_digit(Big *r, const Big *s) {
    int d = 0;
    while (big_cmp(r, s) >= 0) {
        big_sub(r, s);
        d++;
    }
    return d;
}

/* The digits of `x` (finite, nonzero, sign ignored) and the decimal exponent
 * `k` that makes the value `0.DIGITS × 10^k`.
 *
 * `fixed < 0`: the shortest digits that read back as `x`. `fixed >= 0`: every
 * digit up to `fixed` places after the point, rounded half to even on the exact
 * value, as `%.*f` does — which may be none at all, when the value is too small
 * to reach them (answered as zero digits and `k` standing where they would go).
 * Answers how many digits were written into `digits`. */
static int q_digits(nest_u128 x, int fixed, char *digits, int cap, int *k_out) {
    static Big r, s, mp, mm;
    int exp = (int)((x >> Q_SIG_BITS) & Q_EXP_MAX);
    nest_u128 f = x & Q_SIG_MASK;
    int e;
    if (exp == 0) {
        e = 1 - Q_BIAS - Q_SIG_BITS;
    } else {
        f |= Q_IMPLICIT;
        e = exp - Q_BIAS - Q_SIG_BITS;
    }
    /* The gap to the next value down is half the one up when `f` is the
     * least significand of its binade: `r`, `s` and both margins are doubled
     * so that the smaller half-gap is still a whole number. */
    int uneven = exp > 1 && f == Q_IMPLICIT;
    big_set(&r, f);
    big_set(&s, 1);
    big_set(&mp, 1);
    big_set(&mm, 1);
    big_shl(&r, 1 + uneven);
    big_shl(&s, 1 + uneven);
    big_shl(&mp, uneven);
    if (e >= 0) {
        big_shl(&r, e);
        big_shl(&mp, e);
        big_shl(&mm, e);
    } else {
        big_shl(&s, -e);
    }
    /* `k`, estimated from the bit length and then corrected. */
    int bits = 128 - q_clz(f);
    double estimate = (double)(e + bits - 1) * 0.30102999566398119521 - 1e-10;
    int k = (int)estimate;
    if ((double)k < estimate) {
        k++;
    }
    if (k >= 0) {
        big_pow10(&s, k);
    } else {
        big_pow10(&r, -k);
        big_pow10(&mp, -k);
        big_pow10(&mm, -k);
    }
    int even = (int)(f & 1) == 0;
    /* The value (plus its upper margin, in the shortest case) must be below
     * `s`, so that the first digit is the first of the value's own. */
    for (;;) {
        int c = fixed < 0 ? big_cmp_sum(&r, &mp, &s) : big_cmp(&r, &s);
        if (c > 0 || (c == 0 && (fixed >= 0 || even))) {
            big_mul_small(&s, 10);
            k++;
        } else {
            break;
        }
    }
    int n = 0;
    if (fixed >= 0) {
        int want = k + fixed; /* digits before the rounding point */
        for (int i = 0; i < want && n < cap; i++) {
            big_mul_small(&r, 10);
            digits[n++] = (char)('0' + big_digit(&r, &s));
        }
        if (want < 0) {
            *k_out = k;
            return 0;
        }
        /* Round on what is left, `r / s` of one unit in the last place. */
        big_shl(&r, 1);
        int c = big_cmp(&r, &s);
        int up = c > 0 || (c == 0 && (n == 0 ? 0 : (digits[n - 1] - '0') & 1));
        if (want == 0) {
            /* No digit reached: the value rounds to zero or to one unit. */
            up = c > 0;
            if (up) {
                digits[n++] = '1';
                k++;
            }
            *k_out = k;
            return n;
        }
        if (up) {
            int i = n - 1;
            while (i >= 0 && digits[i] == '9') {
                digits[i--] = '0';
            }
            if (i >= 0) {
                digits[i]++;
            } else {
                memmove(digits + 1, digits, (size_t)(n < cap ? n : cap - 1));
                digits[0] = '1';
                if (n < cap) {
                    n++;
                }
                k++;
            }
        }
        *k_out = k;
        return n;
    }
    for (;;) {
        big_mul_small(&r, 10);
        big_mul_small(&mp, 10);
        big_mul_small(&mm, 10);
        int d = big_digit(&r, &s);
        int c1 = big_cmp(&r, &mm);
        int low = c1 < 0 || (c1 == 0 && even);
        int c2 = big_cmp_sum(&r, &mp, &s);
        int high = c2 > 0 || (c2 == 0 && even);
        if (!low && !high) {
            digits[n++] = (char)('0' + d);
            continue;
        }
        if (low && high) {
            big_shl(&r, 1);
            int c = big_cmp(&r, &s);
            if (c > 0 || (c == 0 && (d & 1))) {
                d++;
            }
        } else if (high) {
            d++;
        }
        digits[n++] = (char)('0' + d);
        break;
    }
    *k_out = k;
    return n;
}

/* Append `c` to `out` if there is room; count it either way. */
#define PUT(c)                   \
    do {                         \
        if (len < cap) {         \
            out[len] = (c);      \
        }                        \
        len++;                   \
    } while (0)

/* `x` as `core/fmt.nest` writes a float: the shortest text that reads back
 * as it when `precision` is negative, and that many places after the point
 * otherwise. Writes at most `cap` bytes and answers the length the whole text
 * has — `snprintf`'s contract, so the caller can make room and ask again. */
size_t nest_f128_format(nest_u128 x, int32_t precision, uint8_t *out, size_t cap) {
    static char digits[20000];
    size_t len = 0;
    nest_u128 abs = x & Q_ABS_MASK;
    if (abs > Q_INF) {
        const char *t = "NaN";
        for (; *t; t++) PUT(*t);
        return len;
    }
    if (x & Q_SIGN) {
        PUT('-');
    }
    if (abs == Q_INF) {
        const char *t = "inf";
        for (; *t; t++) PUT(*t);
        return len;
    }
    int k = 1;
    int n = 0;
    if (abs != 0) {
        n = q_digits(abs, precision, digits, (int)sizeof digits, &k);
    }
    if (precision >= 0) {
        /* Positional, `precision` places: the integer part is the digits
         * before `k`, a zero if there are none. */
        if (k <= 0 || n == 0) {
            PUT('0');
        } else {
            for (int i = 0; i < k; i++) PUT(i < n ? digits[i] : '0');
        }
        if (precision > 0) {
            PUT('.');
            for (int i = 0; i < precision; i++) {
                int at = k + i;
                PUT(at >= 0 && at < n ? digits[at] : '0');
            }
        }
        return len;
    }
    if (n == 0) {
        PUT('0');
        return len;
    }
    int e = k - 1; /* the exponent scientific notation would write */
    if (e >= -5 && e < 17) {
        if (e >= 0) {
            for (int i = 0; i <= e; i++) PUT(i < n ? digits[i] : '0');
            if (n > e + 1) {
                PUT('.');
                for (int i = e + 1; i < n; i++) PUT(digits[i]);
            }
        } else {
            PUT('0');
            PUT('.');
            for (int i = 0; i < -e - 1; i++) PUT('0');
            for (int i = 0; i < n; i++) PUT(digits[i]);
        }
        return len;
    }
    PUT(digits[0]);
    if (n > 1) {
        PUT('.');
        for (int i = 1; i < n; i++) PUT(digits[i]);
    }
    PUT('e');
    char ebuf[8];
    int en = snprintf(ebuf, sizeof ebuf, "%d", e);
    for (int i = 0; i < en; i++) PUT(ebuf[i]);
    return len;
}

#undef PUT
