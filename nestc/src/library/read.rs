//! Reading a `.nmeta` into a session.

use std::collections::HashMap;
use std::path::Path;

use crate::sema::session::{FileMeta, Session};

use super::codec::{self, Bases, Decoding};
use super::metas;
use super::{Body, FORMAT, Header, Loaded, MAGIC};

/// The header of the metadata in `bytes`, and the body after it.
pub fn header(bytes: &[u8]) -> Result<(Header, &[u8]), String> {
    let rest = bytes
        .strip_prefix(MAGIC.as_slice())
        .ok_or("not Nest library metadata")?;
    let (len, rest) = rest.split_at_checked(4).ok_or("the metadata is truncated")?;
    let len = u32::from_le_bytes(len.try_into().expect("four bytes")) as usize;
    let (header, body) = rest.split_at_checked(len).ok_or("the metadata is truncated")?;
    let header: Header =
        postcard::from_bytes(header).map_err(|e| format!("the header is unreadable: {e}"))?;
    if header.format != FORMAT {
        return Err(format!(
            "it is metadata format {}, and this compiler reads format {FORMAT}",
            header.format
        ));
    }
    Ok((header, body))
}

/// Read the library in `bytes` into `session`, as a package its files may
/// import when `importable` is set.
///
/// Every package the metadata's ids belong to must already be in the session:
/// a library is read after the libraries it was compiled against.
pub fn load(session: &mut Session, bytes: &[u8], path: &Path, importable: bool) -> Result<(), String> {
    let (header, body) = header(bytes)?;
    if header.compiler != super::compiler_id() {
        return Err(format!(
            "it was compiled by {}, and this is {}; rebuild it",
            header.compiler,
            super::compiler_id()
        ));
    }
    if header.target != session.target_module_source() {
        return Err("it was compiled for a different target or profile; rebuild it".to_string());
    }
    if let Some(existing) = session.libraries.iter_mut().find(|l| l.name == header.name) {
        // The same library named twice — once directly, once as another's
        // dependency — is one library, importable if either said so.
        existing.importable |= importable;
        return Ok(());
    }

    // Where each package in the table starts here.
    let mut packages = Vec::with_capacity(header.packages.len());
    let own = Bases {
        def: session.defs.len() as u32,
        file: session.sources.len() as u32,
        ir: session.ir_meta.reserve(header.counts.ir),
    };
    packages.push(own);
    for name in &header.packages[1..] {
        let lib = session
            .libraries
            .iter()
            .find(|l| &l.name == name)
            .ok_or_else(|| format!("it was compiled against `{name}`, which was not given"))?;
        packages.push(lib.bases);
    }

    // The primitives it names, made before anything refers to them — and made
    // before its defs are placed, so they do not land inside its run.
    let mut builtins = HashMap::new();
    for name in &header.builtins {
        let id = session.defs.intern_primitive(session.builtins, name);
        builtins.insert(name.clone(), id);
    }
    let own = Bases {
        def: session.defs.len() as u32,
        ..own
    };
    packages[0] = own;

    let decoding = Decoding {
        packages,
        builtins,
        missing_builtins: Vec::new(),
    };
    let (body, _) = codec::decode(decoding, || postcard::from_bytes::<Body>(body));
    let body = body.map_err(|e| format!("the metadata is unreadable: {e}"))?;

    let name = header.name.clone();
    for (i, file) in body.files.into_iter().enumerate() {
        let id = session.sources.add(file.name.clone(), file.src);
        if id.0 != own.file + i as u32 {
            return Err(format!("`{}` landed at the wrong file id", file.name));
        }
        metas::import(file.ast.meta_store(), file.facts);
        session.asts.insert(id, file.ast);
        session.files.insert(
            id,
            FileMeta {
                ns: file.ns,
                imports: Vec::new(),
                name: file.name,
            },
        );
        session.pkg_of.insert(id, name.clone());
    }
    for def in body.defs {
        session.defs.push(def)?;
    }
    for (tag, def, from_core) in body.lang_items {
        session.lang_items.set(tag, def, from_core);
    }
    for (file, program) in body.programs {
        session.ir.insert(file, program);
    }
    metas::import(session.ir_meta.store(), body.ir_facts);

    session.adopt_library_root(&name, body.root);
    session.libraries.push(Loaded {
        name,
        bases: own,
        counts: header.counts,
        root: body.root,
        importable,
        path: path.to_path_buf(),
        fingerprint: header.fingerprint,
    });
    Ok(())
}
