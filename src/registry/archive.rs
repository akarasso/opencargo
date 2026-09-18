//! Reading one member out of an uploaded archive under budgets: members
//! scanned, bytes inflated. Pure CPU over bytes already in memory; the caller
//! decides where it runs.

use std::io::Read;

/// What an archive read may cost.
#[derive(Debug, Clone, Copy)]
pub struct Budget {
    pub max_members: usize,
    pub max_member_bytes: u64,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ArchiveError {
    #[error("not a readable archive: {0}")]
    Unreadable(String),
    #[error("archive has more than {0} members")]
    TooManyMembers(usize),
    #[error("archive member {0} is larger than {1} bytes")]
    TooLarge(String, u64),
}

fn bounded(mut r: impl Read, name: &str, max: u64) -> Result<Vec<u8>, ArchiveError> {
    let mut out = Vec::new();
    r.by_ref()
        .take(max + 1)
        .read_to_end(&mut out)
        .map_err(|e| ArchiveError::Unreadable(e.to_string()))?;
    if out.len() as u64 > max {
        return Err(ArchiveError::TooLarge(name.to_string(), max));
    }
    Ok(out)
}

/// The first zip member whose name `wanted` accepts.
pub fn zip_member(
    bytes: &[u8],
    budget: Budget,
    wanted: impl Fn(&str) -> bool,
) -> Result<Option<(String, Vec<u8>)>, ArchiveError> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|e| ArchiveError::Unreadable(e.to_string()))?;
    if archive.len() > budget.max_members {
        return Err(ArchiveError::TooManyMembers(budget.max_members));
    }
    for i in 0..archive.len() {
        let file = archive
            .by_index(i)
            .map_err(|e| ArchiveError::Unreadable(e.to_string()))?;
        let name = file.name().to_string();
        if !wanted(&name) {
            continue;
        }
        if file.size() > budget.max_member_bytes {
            return Err(ArchiveError::TooLarge(name, budget.max_member_bytes));
        }
        let body = bounded(file, &name, budget.max_member_bytes)?;
        return Ok(Some((name, body)));
    }
    Ok(None)
}

/// The first member of a gzipped tarball whose path `wanted` accepts.
pub fn tar_gz_member(
    bytes: &[u8],
    budget: Budget,
    wanted: impl Fn(&str) -> bool,
) -> Result<Option<(String, Vec<u8>)>, ArchiveError> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
    let entries = archive
        .entries()
        .map_err(|e| ArchiveError::Unreadable(e.to_string()))?;
    for (seen, entry) in entries.enumerate() {
        if seen >= budget.max_members {
            return Err(ArchiveError::TooManyMembers(budget.max_members));
        }
        let entry = entry.map_err(|e| ArchiveError::Unreadable(e.to_string()))?;
        let name = entry
            .path()
            .map_err(|e| ArchiveError::Unreadable(e.to_string()))?
            .to_string_lossy()
            .into_owned();
        if !entry.header().entry_type().is_file() || !wanted(&name) {
            continue;
        }
        let body = bounded(entry, &name, budget.max_member_bytes)?;
        return Ok(Some((name, body)));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    const BUDGET: Budget = Budget {
        max_members: 4,
        max_member_bytes: 16,
    };

    fn zip_of(members: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(&mut out);
        for (name, body) in members {
            zip.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
            zip.write_all(body).unwrap();
        }
        zip.finish().unwrap();
        out.into_inner()
    }

    fn tar_gz_of(members: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let gz = flate2::write::GzEncoder::new(&mut out, flate2::Compression::default());
            let mut tar = tar::Builder::new(gz);
            for (name, body) in members {
                let mut header = tar::Header::new_gnu();
                header.set_path(name).unwrap();
                header.set_size(body.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                tar.append(&header, *body).unwrap();
            }
            tar.into_inner().unwrap().finish().unwrap();
        }
        out
    }

    #[test]
    fn a_member_is_read_within_its_budget_or_refused() {
        let zip = zip_of(&[("a/x", b"one"), ("a/METADATA", b"meta")]);
        let found = zip_member(&zip, BUDGET, |n| n.ends_with("/METADATA")).unwrap();
        assert_eq!(found, Some(("a/METADATA".to_string(), b"meta".to_vec())));
        assert_eq!(zip_member(&zip, BUDGET, |n| n == "nope").unwrap(), None);
        let big = zip_of(&[("M", &[0u8; 64])]);
        assert!(matches!(zip_member(&big, BUDGET, |_| true), Err(ArchiveError::TooLarge(..))));
        let many = zip_of(&[("1", b""), ("2", b""), ("3", b""), ("4", b""), ("5", b"")]);
        assert!(matches!(zip_member(&many, BUDGET, |_| false), Err(ArchiveError::TooManyMembers(4))));
        assert!(matches!(zip_member(b"junk", BUDGET, |_| true), Err(ArchiveError::Unreadable(_))));

        let tgz = tar_gz_of(&[("p-1.0/setup.py", b""), ("p-1.0/PKG-INFO", b"info")]);
        let found = tar_gz_member(&tgz, BUDGET, |n| n.ends_with("/PKG-INFO")).unwrap();
        assert_eq!(found.map(|(_, b)| b), Some(b"info".to_vec()));
        let big = tar_gz_of(&[("p/PKG-INFO", &[b'x'; 64])]);
        assert!(matches!(tar_gz_member(&big, BUDGET, |_| true), Err(ArchiveError::TooLarge(..))));
        assert!(tar_gz_member(b"junk", BUDGET, |_| true).is_err());
    }
}
