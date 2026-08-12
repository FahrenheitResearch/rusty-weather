use std::path::Path;

use crate::error::SchedulerResult;

/// Publish scheduler state with the shared durable, same-directory atomic
/// replacement contract used by run manifests and hour metadata.
pub(crate) fn durable_atomic_write(path: &Path, bytes: &[u8]) -> SchedulerResult<()> {
    rw_store::atomic::atomic_write_bytes(path, bytes)?;
    Ok(())
}
