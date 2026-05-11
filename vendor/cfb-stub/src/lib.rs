//! Local stub for the upstream `cfb` crate.
//!
//! Why: Zscaler in our environment blocks `static.crates.io/crates/cfb/0.7.x/download`
//! as a false-positive (the OLE Compound File Binary format trips its malware
//! scanner). Cargo therefore cannot fetch upstream `cfb` from inside the corp
//! network. `cfb` is pulled in transitively via `infer` (file-type sniffing)
//! used by `tauri-utils`. `infer` only calls `cfb::CompoundFile::open(...)`
//! to detect MS Office formats (XLS / DOC / PPT) — none of which CatStage
//! ever needs to identify. Returning `Err` from `open` makes infer skip that
//! branch and fall through to its other matchers.
//!
//! TODO(catcast/cfb-stub): drop this once the upstream crate is allowlisted
//! by IT (or once we move CI/dev off the corp proxy entirely).

use std::io;

/// Minimal stand-in for `cfb::CompoundFile`. Never constructed at runtime —
/// the only entry point, `open`, always returns an error.
pub struct CompoundFile<F> {
    _phantom: std::marker::PhantomData<F>,
}

impl<F: io::Read + io::Seek> CompoundFile<F> {
    /// Stubbed: always returns `Err`. Real `cfb` would parse the OLE header.
    pub fn open(_reader: F) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cfb-stub: OLE parsing disabled in this build",
        ))
    }

    pub fn root_entry(&self) -> Entry {
        Entry
    }
}

/// Stub for `cfb::Entry`. `root_entry()` is the only access path infer uses.
pub struct Entry;

impl Entry {
    pub fn clsid(&self) -> Clsid {
        Clsid
    }
}

/// Stub for `cfb`'s CLSID type. infer just calls `.to_string()` on it.
pub struct Clsid;

impl std::fmt::Display for Clsid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("00000000-0000-0000-0000-000000000000")
    }
}
