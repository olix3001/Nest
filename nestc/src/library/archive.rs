//! `.nlib`: a Unix `ar` archive holding a package's metadata and its objects.
//!
//! The format is the common one — a global header, then each member behind a
//! 60-byte header of fixed-width text fields — written by hand because it is
//! that small. There is no symbol table: a linker is never given the archive,
//! only the objects read back out of it, so nothing needs to search it.

use std::path::Path;

const GLOBAL: &[u8] = b"!<arch>\n";

/// The name the metadata member is stored under.
pub const METADATA: &str = "nest.nmeta";

/// An archive of `members`, each a name and its bytes. A name is at most 15
/// bytes and has no `/` in it.
pub fn write(members: &[(String, Vec<u8>)]) -> Result<Vec<u8>, String> {
    let mut out = GLOBAL.to_vec();
    for (name, data) in members {
        if name.len() > 15 || name.contains('/') {
            return Err(format!("`{name}` is not a name an archive member can have"));
        }
        let header = format!(
            "{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
            name,
            0,
            0,
            0,
            644,
            data.len()
        );
        debug_assert_eq!(header.len(), 60);
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(data);
        if data.len() % 2 == 1 {
            out.push(b'\n');
        }
    }
    Ok(out)
}

/// The members of the archive in `bytes`, in order.
pub fn read(bytes: &[u8]) -> Result<Vec<(String, &[u8])>, String> {
    let mut rest = bytes.strip_prefix(GLOBAL).ok_or("not an archive")?;
    let mut members = Vec::new();
    while !rest.is_empty() {
        let (header, after) = rest.split_at_checked(60).ok_or("the archive is truncated")?;
        let text = std::str::from_utf8(header).map_err(|_| "a member header is not text")?;
        let name = text[..16].trim_end().trim_end_matches('/').to_string();
        let size: usize = text[48..58]
            .trim()
            .parse()
            .map_err(|_| format!("the member `{name}` has no size"))?;
        let (data, after) = after.split_at_checked(size).ok_or("the archive is truncated")?;
        members.push((name, data));
        rest = if size % 2 == 1 { after.get(1..).unwrap_or(&[]) } else { after };
    }
    Ok(members)
}

/// The metadata inside the library at `path` — or the file itself, when it is a
/// bare `.nmeta`.
pub fn metadata_of(path: &Path) -> Result<Vec<u8>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read `{}`: {e}", path.display()))?;
    if bytes.starts_with(GLOBAL) {
        let members = read(&bytes).map_err(|e| format!("`{}`: {e}", path.display()))?;
        return members
            .into_iter()
            .find(|(name, _)| name == METADATA)
            .map(|(_, data)| data.to_vec())
            .ok_or_else(|| format!("`{}` holds no metadata", path.display()));
    }
    Ok(bytes)
}

/// The objects inside the library at `path`, by member name.
pub fn objects_of(path: &Path) -> Result<Vec<(String, Vec<u8>)>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read `{}`: {e}", path.display()))?;
    if !bytes.starts_with(GLOBAL) {
        return Ok(Vec::new());
    }
    Ok(read(&bytes)
        .map_err(|e| format!("`{}`: {e}", path.display()))?
        .into_iter()
        .filter(|(name, _)| name != METADATA)
        .map(|(name, data)| (name, data.to_vec()))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn members_round_trip_including_odd_sizes() {
        let members = vec![
            ("nest.nmeta".to_string(), b"abc".to_vec()),
            ("unit0.o".to_string(), b"odd!!".to_vec()),
            ("unit1.o".to_string(), b"even".to_vec()),
        ];
        let bytes = write(&members).unwrap();
        let back = read(&bytes).unwrap();
        assert_eq!(back.len(), 3);
        for ((n, d), (bn, bd)) in members.iter().zip(back) {
            assert_eq!(n, &bn);
            assert_eq!(d.as_slice(), bd);
        }
    }
}
