//! Atomic file writes ported from rustwx-products/src/publication.rs
//! (`atomic_write_bytes` / `temp_path_for`, rustwx-fastplots-wt): write to a
//! hidden temp file in the same directory, fsync, atomically replace the
//! destination, and make the directory entry durable. The temp file is
//! removed on any failure without deleting the previous destination first.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::RwResult;

/// Temp-file name in the same directory as `path`: `.{file_name}.tmp-{pid}-{seq}`.
/// The original used a millisecond timestamp for the last component; a process
/// counter gives the same same-directory/same-volume rename guarantee while
/// staying unique under rapid successive calls within one process.
fn temp_path_for(path: &Path) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("artifact");
    path.with_file_name(format!(
        ".{file_name}.tmp-{}-{}",
        process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Write `bytes` to `path` atomically: the destination either keeps its old
/// content or holds exactly `bytes`, never a partial write. Parent
/// directories are created as needed.
pub fn atomic_write_bytes(path: &Path, bytes: &[u8]) -> RwResult<()> {
    atomic_write_with(path, |writer| {
        writer.write_all(bytes)?;
        Ok(())
    })
}

/// Streaming sibling of [`atomic_write_bytes`]: the caller writes through a
/// buffered handle on the hidden temp file instead of materializing the
/// whole payload in memory first. Identical guarantees — create-new temp in
/// the same directory, fsync, rename into place, temp removed on any
/// failure — so the destination either keeps its old content or holds
/// exactly what `write` produced.
pub fn atomic_write_with<F>(path: &Path, write: F) -> RwResult<()>
where
    F: FnOnce(&mut io::BufWriter<fs::File>) -> RwResult<()>,
{
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let (tmp_path, file) = loop {
        let candidate = temp_path_for(path);
        match fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&candidate)
        {
            Ok(file) => break (candidate, file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    };
    let write_result = (|| -> RwResult<()> {
        let mut writer = io::BufWriter::with_capacity(1 << 20, file);
        write(&mut writer)?;
        writer.flush()?;
        writer
            .into_inner()
            .map_err(|err| err.into_error())?
            .sync_all()?;
        Ok(())
    })();
    if let Err(err) = write_result {
        let _ = fs::remove_file(&tmp_path);
        return Err(err);
    }
    let finalize_result = (|| -> RwResult<()> {
        replace_file(&tmp_path, path)?;
        sync_parent(path)?;
        Ok(())
    })();
    if let Err(err) = finalize_result {
        let _ = fs::remove_file(&tmp_path);
        return Err(err);
    }
    Ok(())
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    use std::ffi::c_void;
    use std::mem::{offset_of, size_of};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::ptr;
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_FLAG_WRITE_THROUGH, FILE_RENAME_INFO, FILE_RENAME_INFO_0, FileRenameInfoEx,
        SetFileInformationByHandle,
    };
    use windows_sys::Win32::System::WindowsProgramming::{
        FILE_RENAME_FLAG_POSIX_SEMANTICS, FILE_RENAME_FLAG_REPLACE_IF_EXISTS,
    };

    // The ordinary rename is the correct path for first publication. It also
    // closes the race where another writer creates the destination between an
    // existence check and this operation: that case simply falls through to
    // the replacement path below.
    match fs::rename(source, destination) {
        Ok(()) => return Ok(()),
        Err(error) if !source.exists() => return Err(error),
        Err(_) => {}
    }

    // MoveFileExW(REPLACE_EXISTING) cannot replace an actively memory-mapped
    // destination on Windows. FILE_RENAME_INFO_EX with POSIX semantics is the
    // supported atomic namespace swap: existing readers keep their old file
    // object while new opens resolve to the replacement. Rust File handles
    // opt into FILE_SHARE_DELETE, which is required by this operation.
    let destination = std::path::absolute(destination)?;
    let destination_wide = destination.as_os_str().encode_wide().collect::<Vec<_>>();
    let name_bytes = destination_wide
        .len()
        .checked_mul(size_of::<u16>())
        .ok_or_else(|| io::Error::other("destination path is too long"))?;
    // FileNameLength excludes the terminator, but keeping a trailing UTF-16
    // NUL avoids filesystem/driver implementations reading uninitialized
    // padding beyond the variable-length name.
    let buffer_bytes = offset_of!(FILE_RENAME_INFO, FileName)
        .checked_add(name_bytes)
        .and_then(|bytes| bytes.checked_add(size_of::<u16>()))
        .ok_or_else(|| io::Error::other("rename buffer length overflow"))?;
    let words = buffer_bytes.div_ceil(size_of::<usize>());
    let mut buffer = vec![0usize; words];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    // Open the already-fsynced temp with DELETE access so Windows permits a
    // handle-based rename. access_mode overrides the read/write builder bits.
    let source_file = fs::OpenOptions::new()
        .access_mode(DELETE)
        .custom_flags(FILE_FLAG_WRITE_THROUGH)
        .open(source)?;
    // SAFETY: the buffer is usize-aligned and large enough for the fixed
    // header plus the exact UTF-16 name payload. The source handle and buffer
    // remain live for the entire call.
    let replaced = unsafe {
        (*info).Anonymous = FILE_RENAME_INFO_0 {
            Flags: FILE_RENAME_FLAG_REPLACE_IF_EXISTS | FILE_RENAME_FLAG_POSIX_SEMANTICS,
        };
        (*info).RootDirectory = ptr::null_mut();
        (*info).FileNameLength = u32::try_from(name_bytes)
            .map_err(|_| io::Error::other("destination path is too long"))?;
        ptr::copy_nonoverlapping(
            destination_wide.as_ptr(),
            (*info).FileName.as_mut_ptr(),
            destination_wide.len(),
        );
        SetFileInformationByHandle(
            source_file.as_raw_handle() as _,
            FileRenameInfoEx,
            info.cast::<c_void>(),
            u32::try_from(buffer_bytes)
                .map_err(|_| io::Error::other("rename buffer is too large"))?,
        )
    };
    if replaced == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(not(windows))]
fn sync_parent(path: &Path) -> io::Result<()> {
    match path.parent() {
        Some(parent) => fs::File::open(parent)?.sync_all(),
        None => Ok(()),
    }
}

#[cfg(windows)]
fn sync_parent(_path: &Path) -> io::Result<()> {
    // The handle used by replace_file is opened with FILE_FLAG_WRITE_THROUGH.
    // Windows applies that policy to metadata updates caused by the rename.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rw-store-atomic-{}-{}", process::id(), name));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn tmp_entries(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp"))
            .collect()
    }

    #[test]
    fn writes_new_file_with_exact_content() {
        let dir = test_dir("new-file");
        let path = dir.join("nested").join("out.rws");
        atomic_write_bytes(&path, b"hello rw-store").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"hello rw-store");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn overwrites_existing_file() {
        let dir = test_dir("overwrite");
        let path = dir.join("out.rws");
        fs::write(&path, b"old content that is longer").unwrap();
        atomic_write_bytes(&path, b"new").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn leaves_no_temp_files_after_success() {
        let dir = test_dir("no-temp-success");
        let path = dir.join("out.rws");
        atomic_write_bytes(&path, b"first").unwrap();
        atomic_write_bytes(&path, b"second").unwrap();
        assert_eq!(
            tmp_entries(&dir),
            Vec::<String>::new(),
            "no .tmp files should remain after successful writes"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cleans_up_temp_file_when_target_is_a_directory() {
        let dir = test_dir("target-is-dir");
        let path = dir.join("out.rws");
        // Make the destination un-replaceable: a directory cannot be
        // remove_file'd or renamed over, so finalize must fail.
        fs::create_dir_all(&path).unwrap();
        let err = atomic_write_bytes(&path, b"doomed").unwrap_err();
        assert!(
            matches!(err, crate::RwStoreError::Io(_)),
            "expected Io error, got {err:?}"
        );
        assert!(path.is_dir(), "destination directory must be untouched");
        assert_eq!(
            tmp_entries(&dir),
            Vec::<String>::new(),
            "temp file must be cleaned up after failure"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_replacements_publish_one_complete_value() {
        let dir = test_dir("concurrent-replace");
        let path = dir.join("state.json");
        atomic_write_bytes(&path, b"initial").unwrap();
        let writers = (0..8)
            .map(|index| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let payload = format!("writer-{index}-{}", "x".repeat(32 * 1024));
                    atomic_write_bytes(&path, payload.as_bytes()).unwrap();
                })
            })
            .collect::<Vec<_>>();
        for writer in writers {
            writer.join().unwrap();
        }
        let value = fs::read_to_string(&path).unwrap();
        assert!(value.starts_with("writer-"));
        assert_eq!(value.len(), "writer-0-".len() + 32 * 1024);
        assert_eq!(tmp_entries(&dir), Vec::<String>::new());
        let _ = fs::remove_dir_all(&dir);
    }
}
