/*
 * The Nest runtime, in one file.
 *
 * Every symbol a generated object file refers to that is not a Nest function is
 * here, and there are sixteen of them. That is the point: the language's runtime
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
#include <math.h>
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

/* ===< A float as text >===
 *
 * `core/fmt` writes every other value itself and does not write this one. The
 * shortest decimal that reads back as the same bits is Ryū or Grisu — a table
 * and a few hundred lines — and libc already answers the question, so the
 * search is here rather than there.
 *
 * It is *here* rather than in Nest because of the call: `snprintf` is variadic,
 * and on arm64 a variadic argument is passed differently from a fixed one, so a
 * Nest declaration of it as an ordinary function reads whatever the stack held.
 * C is where a variadic call is an ordinary call.
 *
 * `precision` is the `.3` of a format specifier and **-1 means none was
 * written**, which asks for the shortest text instead: one significant digit,
 * then two, until `strtod` reads back the value that was handed in.
 *
 * Both write at most `cap` bytes and return the length of the **whole** text,
 * the way `snprintf` does — so a caller whose buffer was too small is told
 * exactly how much to reserve and asks again. A caller is told nothing about
 * NUL terminators: a Nest `str` is a pointer and a length, and the byte after
 * the text is the caller's business.
 *
 * The locale is the C one, which is what a process starts in: nothing here
 * calls `setlocale`, so the decimal point is a `.`.
 */

/* Room for any text the shortest form produces: seventeen significant digits,
 * a sign, a point, the leading zeroes of the smallest positional form, and the
 * NUL `snprintf` writes. The bounds below are what keep it true. */
#define NEST_FMT_ROOM 64

/* The decimal exponents between which a value is written positionally rather
 * than in scientific notation: `0.00001` and `100` are written out, `1e-7` and
 * `1e17` are not. Outside this range the positional form is almost all zeroes —
 * `1e300` is three hundred of them — and the exponent is what a reader wants. */
#define NEST_FMT_MIN_EXP (-5)
#define NEST_FMT_MAX_EXP 17

/* `text` into the caller's buffer, when all of it fits.
 *
 * Nothing partial is ever written: a caller that is told a length longer than
 * the room it offered reserves that much and asks again, and a half-written
 * value in between would be a value it could mistake for the whole. */
static size_t nest_fmt_emit(unsigned char *buf, size_t cap, const char *text, size_t len) {
    if (len < cap) {
        memcpy(buf, text, len);
    }
    return len;
}

/* `1.5e+07` as `1.5e7`, `1e-07` as `1e-7`.
 *
 * C pads an exponent to two digits and writes a `+` on a positive one. Neither
 * is what a program would type, and the text here is the text a program reads.
 * `out` is a `NEST_FMT_ROOM` buffer and `sci` came out of one, so the copy is
 * bounded by the source. */
static size_t nest_fmt_tidy(const char *sci, char *out) {
    const char *e = strchr(sci, 'e');
    size_t len = (size_t)(e - sci);
    memcpy(out, sci, len);
    out[len++] = 'e';
    const char *d = e + 1;
    if (*d == '-') {
        out[len++] = '-';
        d++;
    } else if (*d == '+') {
        d++;
    }
    while (d[0] == '0' && d[1] != '\0') {
        d++;
    }
    while (*d != '\0') {
        out[len++] = *d++;
    }
    return len;
}

/* The body both widths share. `single` says the value came from an `f32`, which
 * changes only what counts as reading back the same value — and so how many
 * digits are needed to. */
static size_t nest_fmt_float(unsigned char *buf, size_t cap, double value, int32_t precision,
                             int single) {
    char sci[NEST_FMT_ROOM];
    char text[NEST_FMT_ROOM];
    int n;

    /* Spelled as Nest spells them, not as C does: `%f` writes `nan` and `inf`
     * in a case that has varied between platforms. */
    if (isnan(value)) {
        return nest_fmt_emit(buf, cap, "NaN", 3);
    }
    if (isinf(value)) {
        return value < 0 ? nest_fmt_emit(buf, cap, "-inf", 4) : nest_fmt_emit(buf, cap, "inf", 3);
    }

    /* A written precision is the whole answer: `{x:.3}` is three digits after
     * the point, however many that makes in front of it. The length is asked
     * for first because there is no bound on it — `{1e300:.2}` is three hundred
     * and four characters — so there is no buffer here that would always do. */
    if (precision >= 0) {
        n = snprintf(NULL, 0, "%.*f", (int)precision, value);
        if (n < 0) {
            return 0;
        }
        if ((size_t)n < cap) {
            snprintf((char *)buf, cap, "%.*f", (int)precision, value);
        }
        return (size_t)n;
    }

    /* The shortest text that reads back as the value it was given. Seventeen
     * significant digits always suffice for a double and nine for a float, so
     * the loop below always ends with a text that round-trips. */
    int most = single ? 9 : 17;
    int digits = 1;
    for (; digits < most; digits++) {
        snprintf(sci, sizeof sci, "%.*e", digits - 1, value);
        if (single ? strtof(sci, NULL) == (float)value : strtod(sci, NULL) == value) {
            break;
        }
    }
    if (digits == most) {
        snprintf(sci, sizeof sci, "%.*e", most - 1, value);
    }

    /* `%e` always writes an exponent, and it says where the point belongs. */
    const char *e = strchr(sci, 'e');
    if (e == NULL) {
        return nest_fmt_emit(buf, cap, sci, strlen(sci));
    }
    int exp10 = atoi(e + 1);

    if (exp10 >= NEST_FMT_MIN_EXP && exp10 < NEST_FMT_MAX_EXP) {
        /* The same digits with the point written where it falls. Asking `%f`
         * for exactly the ones that land after it writes the number the search
         * above found and no more: `1e+02` becomes `100`, `1e-01` becomes
         * `0.1`, and a value that needs none of them keeps none. */
        int frac = digits - 1 - exp10;
        if (frac < 0) {
            frac = 0;
        }
        n = snprintf(text, sizeof text, "%.*f", frac, value);
        if (n < 0) {
            return 0;
        }
        return nest_fmt_emit(buf, cap, text, (size_t)n);
    }
    return nest_fmt_emit(buf, cap, text, nest_fmt_tidy(sci, text));
}

size_t nest_fmt_f64(unsigned char *buf, size_t cap, double value, int32_t precision) {
    return nest_fmt_float(buf, cap, value, precision, 0);
}

/* The double is the float exactly — every `f32` is an `f64` — so only the
 * round-trip comparison has to know which width was asked about. */
size_t nest_fmt_f32(unsigned char *buf, size_t cap, float value, int32_t precision) {
    return nest_fmt_float(buf, cap, (double)value, precision, 1);
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
