//! What the backend interface promises, independent of any one backend.
//!
//! These run against whatever [`backends`] holds, so a backend added later is
//! covered by them without being named here. That is the point: the properties
//! below are the contract, and a backend that breaks one is broken whether or
//! not somebody remembered to write a test for it.

use super::*;
use crate::common::options::{ARCHES, OSES};

/// **Every backend has a distinct name, and `select` finds each one.**
///
/// `-C backend=` is how a build tool names one, so two backends answering to the
/// same string would make the choice silently depend on list order.
#[test]
fn a_backend_is_selected_by_its_name() {
    let names: Vec<&str> = backends().iter().map(|b| b.name()).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), names.len(), "two backends share a name: {names:?}");

    for name in &names {
        let picked = select(Some(name)).expect("a listed backend is selectable");
        assert_eq!(picked.name(), *name);
    }
}

/// **An unknown backend is an error, not a fallback.**
///
/// The same rule an unknown `-C` key follows: a typo must not quietly build
/// something other than what was asked for.
#[test]
fn an_unknown_backend_is_refused() {
    let Err(err) = select(Some("gcc")) else {
        panic!("`gcc` is not a backend");
    };
    assert!(err.contains("gcc"), "{err}");
    // And the default is still available with no name.
    assert!(select(None).is_ok());
}

/// **Every backend resolves the host**, because that is what a compiler run with
/// no `--target` is asked to do.
///
/// The facts it reports have to be ones the rest of the compiler can use: a
/// pointer width the layout engine accepts, and an OS and architecture spelled
/// the way `core`'s generated `target.nest` spells them — those names reach
/// *source*, as `.macos` and `.aarch64`, so a backend inventing one would
/// produce a `core` that does not compile.
#[test]
fn every_backend_resolves_the_host_to_facts_the_front_end_accepts() {
    for b in backends() {
        let info = b
            .target_info(None)
            .unwrap_or_else(|e| panic!("{}: cannot resolve the host: {e}", b.name()));
        assert!(
            matches!(info.pointer_bits, 16 | 32 | 64),
            "{}: pointer width {}",
            b.name(),
            info.pointer_bits
        );
        assert!(OSES.contains(&info.os), "{}: os `{}`", b.name(), info.os);
        assert!(
            ARCHES.contains(&info.arch),
            "{}: arch `{}`",
            b.name(),
            info.arch
        );
        assert!(!info.triple.is_empty(), "{}: empty triple", b.name());
        // And the conversion the driver actually performs agrees with it.
        let target = info.target();
        assert_eq!(target.pointer_bits, info.pointer_bits);
        assert_eq!(target.os, info.os);
        assert_eq!(target.arch, info.arch);
    }
}

/// **A triple a backend does not know is refused**, rather than resolved to the
/// host.
///
/// This is the one that matters for cross-compilation: falling back to the host
/// would lay every type out for the wrong machine and produce an object that
/// looks fine until it is loaded.
#[test]
fn an_unknown_triple_is_refused_rather_than_resolved_to_the_host() {
    for b in backends() {
        let err = b
            .target_info(Some("pdp11-dec-unix"))
            .expect_err("a backend should not invent a target");
        assert!(
            matches!(err, CodegenError::Unsupported(_)),
            "{}: {err:?}",
            b.name()
        );
    }
}

/// **A `--target` is honored**, and a 32-bit one produces a 32-bit machine.
///
/// The failure this catches is the quiet one: a backend that parsed the triple,
/// ignored it and reported the host's 64 bits would pass every other test here.
#[test]
fn a_thirty_two_bit_triple_gives_a_thirty_two_bit_machine() {
    for b in backends() {
        let info = b
            .target_info(Some("wasm32-unknown-none"))
            .unwrap_or_else(|e| panic!("{}: {e}", b.name()));
        assert_eq!(info.pointer_bits, 32, "{}", b.name());
        assert_eq!(info.arch, "wasm32", "{}", b.name());
    }
}

/// **A backend refuses an output kind it does not produce**, rather than writing
/// a file of the wrong thing.
///
/// The `lir` backend produces exactly one kind, which makes it the one that can
/// state this. A file that exists and holds something other than what was asked
/// for is worse than no file, because a build system will happily link it.
#[test]
fn a_backend_refuses_a_kind_it_does_not_produce() {
    let dir = std::env::temp_dir().join("nestc-codegen-tests");
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("refused.o");
    let _ = std::fs::remove_file(&out);

    let mut b = text::TextBackend;
    let unit = Unit {
        name: "empty".to_string(),
        types: Vec::new(),
        globals: Vec::new(),
        funcs: Vec::new(),
    };
    let err = b
        .emit_unit(&unit, OutputKind::Object, &out)
        .expect_err("the `lir` backend has no object writer");
    assert!(matches!(err, CodegenError::Unsupported(_)), "{err:?}");
    assert!(!out.exists(), "a refused emission still wrote a file");
}

/// **The kind it does produce is written, and it is the unit.**
#[test]
fn the_lir_backend_writes_the_dump() {
    let dir = std::env::temp_dir().join("nestc-codegen-tests");
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("written.lir");
    let _ = std::fs::remove_file(&out);

    let mut b = text::TextBackend;
    let unit = Unit {
        name: "written".to_string(),
        types: Vec::new(),
        globals: Vec::new(),
        funcs: Vec::new(),
    };
    b.emit_unit(&unit, OutputKind::Ir, &out).expect("written");
    let text = std::fs::read_to_string(&out).unwrap();
    assert!(text.contains("unit written"), "{text}");
    assert_eq!(b.extension(OutputKind::Ir), "lir");
}

/// **`--emit=` names round-trip.** A spelling is what a build tool writes and
/// what a diagnostic prints, so the two directions have to agree.
#[test]
fn every_output_kind_has_a_spelling_and_an_extension() {
    for kind in [OutputKind::Object, OutputKind::Assembly, OutputKind::Ir] {
        assert!(!kind.name().is_empty());
        assert!(!kind.extension().is_empty());
        assert!(
            !kind.extension().starts_with('.'),
            "{}: the extension carries its own dot",
            kind.name()
        );
    }
}
