use crate::relay::{SNAPSHOT_VERSION, Snapshot};
use anyhow::{Context, Result, bail};
use std::io::Write;
use std::path::Path;

/// Reads the state file; a missing file is a fresh start, anything unreadable is an error.
pub fn load(path: &Path) -> Result<Option<Snapshot>> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading state file {}", path.display())),
    };
    let bad = || format!("state file {} is not valid; fix or move it aside", path.display());
    let v: serde_json::Value = serde_json::from_slice(&bytes).with_context(bad)?;
    let version = v.get("version").and_then(|v| v.as_u64());
    if version != Some(SNAPSHOT_VERSION.into()) {
        bail!(
            "state file {} has format version {}, this amx reads version {SNAPSHOT_VERSION}",
            path.display(),
            version.map_or("none".to_string(), |v| v.to_string())
        );
    }
    Ok(Some(serde_json::from_value(v).with_context(bad)?))
}

/// Writes through a temporary file in the same directory, then renames it over `path`.
pub fn save(path: &Path, snap: &Snapshot) -> Result<()> {
    let ctx = || format!("writing state file {}", path.display());
    let name = path.file_name().with_context(ctx)?.to_string_lossy();
    let tmp = path.with_file_name(format!(".{name}.tmp"));
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(ctx)?;
    }
    let bytes = serde_json::to_vec(snap).with_context(ctx)?;
    let mut f = std::fs::File::create(&tmp).with_context(ctx)?;
    f.write_all(&bytes).with_context(ctx)?;
    f.sync_all().with_context(ctx)?;
    std::fs::rename(&tmp, path).with_context(ctx)
}
