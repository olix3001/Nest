//! Writing a package's library: the `nest.nmeta` and `nest.nir` members of its
//! `.nlib`.

use std::collections::HashMap;

use serde::Serialize;

use crate::common::source::FileId;
use crate::common::symbol::Symbol;
use crate::ir::{IrId, Program};

use crate::sema::decl::Decl;
use crate::sema::def::{Def, DefId, DefKind};
use crate::sema::impls::ImplInfo;
use crate::sema::session::Session;

use super::codec::{self, Counts, Encoding};
use super::metas::{self, MetaValue};
use super::{FORMAT, Header, MAGIC};

/// [`super::Meta`], borrowed: what is written is the session's own tables, and
/// copying a package's every tree to write it would be the whole cost of
/// writing. The field order is the member's, which is what the format is.
#[derive(Serialize)]
struct Meta<'a> {
    files: Vec<FileRecord<'a>>,
    root: FileId,
    defs: Vec<&'a Def>,
    decls: Vec<(DefId, &'a Decl)>,
    impls: Vec<&'a ImplInfo>,
    lang_items: Vec<(Symbol, DefId, bool)>,
}

/// [`super::Ir`], borrowed.
#[derive(Serialize)]
struct Ir<'a> {
    programs: Vec<(FileId, &'a Program)>,
    facts: Vec<(IrId, MetaValue)>,
}

#[derive(Serialize)]
struct FileRecord<'a> {
    name: &'a str,
    src: &'a str,
    ns: DefId,
}

/// The library members of `package`, which `session` compiled from source with
/// `root` as its root file: its metadata and its IR, in that order.
///
/// Both are written under **one** encoding, because the ids in them are the
/// same ids and the header that says how to translate them is written once.
///
/// Everything the session holds that did not come from a library is the
/// package's, so this is only meaningful for a session whose entry was the
/// package's root: a program's entry file belongs to no package, and is refused.
pub fn members(
    session: &Session,
    package: &str,
    root: FileId,
) -> Result<(Vec<u8>, Vec<u8>), String> {
    // The package's own files, in id order, numbered from zero.
    let mut own_files: HashMap<FileId, u32> = HashMap::new();
    let mut files = Vec::new();
    for file in session.sources.files() {
        if session.is_foreign_file(file.id) {
            continue;
        }
        match session.pkg_of.get(&file.id) {
            Some(p) if p == package => {}
            _ => {
                return Err(format!(
                    "`{}` is not part of the package `{package}`, so it has no place in its metadata",
                    file.name
                ));
            }
        }
        own_files.insert(file.id, files.len() as u32);
        files.push(file);
    }

    // Its own defs: everything that is neither a library's nor a primitive.
    let mut builtins: HashMap<DefId, Symbol> = HashMap::new();
    let mut own_defs: HashMap<DefId, u32> = HashMap::new();
    let mut defs = Vec::new();
    for def in session.defs.iter() {
        let primitive = def.id == session.builtins
            || (def.parent == Some(session.builtins) && def.kind == DefKind::Primitive);
        if primitive {
            builtins.insert(def.id, def.name.clone());
            continue;
        }
        if session.is_foreign_def(def.id) || def.id.0 >= session.defs_before_mono {
            continue;
        }
        if let Some(file) = def.file
            && !own_files.contains_key(&file)
        {
            return Err(format!(
                "`{}` is declared in a file outside the package `{package}`",
                def.name
            ));
        }
        own_defs.insert(def.id, defs.len() as u32);
        defs.push(def);
    }

    // The package's own impls: what a package compiled against this one selects
    // over, and the half of an impl that has no tree to be read from there.
    let own_impls: Vec<&ImplInfo> = session
        .impls
        .impls
        .iter()
        .filter(|i| own_files.contains_key(&i.file))
        .collect();

    let mut packages = vec![package.to_string()];
    let foreign = session
        .libraries
        .iter()
        .map(|lib| {
            packages.push(lib.name.clone());
            ((packages.len() - 1) as u32, lib.bases, lib.counts)
        })
        .collect();

    let own_ir_base = session.own_ir_base;
    let ir_end = session.ir_before_mono.max(own_ir_base);
    let encoding = Encoding {
        own_defs,
        own_files,
        own_ir_base,
        foreign,
        builtins,
        unowned: Vec::new(),
        builtins_used: Vec::new(),
    };

    let (bodies, encoding) = codec::encode(encoding, || -> Result<(Vec<u8>, Vec<u8>), String> {
        let mut records = Vec::with_capacity(files.len());
        for file in &files {
            let meta = session
                .files
                .get(&file.id)
                .ok_or_else(|| format!("`{}` was never collected", file.name))?;
            records.push(FileRecord {
                name: &file.name,
                src: &file.src,
                ns: meta.ns,
            });
        }
        let meta = Meta {
            files: records,
            root,
            defs: defs.clone(),
            decls: defs
                .iter()
                .filter_map(|d| session.decls.get(&d.id).map(|decl| (d.id, decl)))
                .collect(),
            impls: own_impls.clone(),
            lang_items: session
                .lang_items
                .claims()
                .filter(|(_, def, _)| !session.is_foreign_def(*def))
                .map(|(tag, def, core)| (tag.clone(), def, core))
                .collect(),
        };
        let ir = Ir {
            programs: files
                .iter()
                .filter_map(|f| session.ir.get(&f.id).map(|p| (f.id, p)))
                .collect(),
            facts: metas::export(session.ir_meta.store(), |id| {
                id.0 >= own_ir_base && id.0 < ir_end
            })?,
        };
        let meta = postcard::to_stdvec(&meta)
            .map_err(|e| format!("cannot serialize the metadata: {e}"))?;
        let ir = postcard::to_stdvec(&ir).map_err(|e| format!("cannot serialize the IR: {e}"))?;
        Ok((meta, ir))
    });
    let (body, ir) = bodies?;
    if !encoding.unowned.is_empty() {
        return Err(format!(
            "the metadata names ids nothing owns: {}",
            encoding.unowned.join(", ")
        ));
    }

    // The files as they are on disk, which is what checking freshness will read:
    // a file with no such path — `core`'s generated `target.nest` — is covered
    // by the target the header carries instead.
    let inputs: Vec<&str> = files
        .iter()
        .filter(|f| std::path::Path::new(&f.name).is_file())
        .map(|f| f.name.as_str())
        .collect();
    let target = session.target_module_source();
    let settings = session.options.render();
    let fingerprint = super::fingerprint::compute(&super::fingerprint::Inputs {
        compiler: &super::compiler_id(),
        target: &target,
        settings: &settings,
        package,
        files: files
            .iter()
            .filter(|f| inputs.contains(&f.name.as_str()))
            .map(|f| (f.name.as_str(), f.src.as_bytes()))
            .collect(),
        libraries: session
            .libraries
            .iter()
            .map(|l| (l.name.as_str(), l.fingerprint, l.importable))
            .collect(),
    });

    let header = Header {
        format: FORMAT,
        compiler: super::compiler_id(),
        name: package.to_string(),
        target,
        packages,
        counts: Counts {
            defs: defs.len() as u32,
            files: files.len() as u32,
            ir: ir_end - own_ir_base,
        },
        builtins: encoding.builtins_used,
        inputs: inputs.iter().map(|s| s.to_string()).collect(),
        settings,
        fingerprint,
    };
    let header =
        postcard::to_stdvec(&header).map_err(|e| format!("cannot serialize the header: {e}"))?;

    let mut out = Vec::with_capacity(MAGIC.len() + 4 + header.len() + body.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(header.len() as u32).to_le_bytes());
    out.extend_from_slice(&header);
    out.extend_from_slice(&body);
    Ok((out, ir))
}
