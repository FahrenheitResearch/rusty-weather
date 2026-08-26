// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Signature {
    bytes: u64,
    modified: Option<SystemTime>,
}

#[derive(Debug, Clone)]
struct Observation {
    signature: Signature,
    stable_polls: usize,
    emitted: Option<Signature>,
}

/// Poll-based publication detector for `frames_per_outfile=1` WRF output.
/// A path is returned only after its size/mtime have remained unchanged for
/// the configured number of polls and its last write is older than `min_age`.
/// If the same path later changes it is eligible again, which keeps recovery
/// deterministic without treating an open NetCDF file as complete.
#[derive(Debug)]
pub struct StableWrfoutWatcher {
    observations: BTreeMap<PathBuf, Observation>,
    required_stable_polls: usize,
    min_age: Duration,
    max_depth: usize,
}

impl Default for StableWrfoutWatcher {
    fn default() -> Self {
        Self::new(3, Duration::from_secs(2))
    }
}

impl StableWrfoutWatcher {
    pub fn new(required_stable_polls: usize, min_age: Duration) -> Self {
        Self {
            observations: BTreeMap::new(),
            required_stable_polls: required_stable_polls.max(1),
            min_age,
            max_depth: 4,
        }
    }

    pub fn scan(&mut self, root: &Path) -> Result<Vec<PathBuf>> {
        let now = SystemTime::now();
        let mut paths = Vec::new();
        collect_wrfout(root, 0, self.max_depth, &mut paths)?;
        paths.sort();
        let mut ready = Vec::new();
        for path in paths {
            let metadata = fs::metadata(&path)?;
            if metadata.len() == 0 {
                continue;
            }
            let signature = Signature {
                bytes: metadata.len(),
                modified: metadata.modified().ok(),
            };
            let observation = self
                .observations
                .entry(path.clone())
                .or_insert(Observation {
                    signature,
                    stable_polls: 0,
                    emitted: None,
                });
            if observation.signature == signature {
                observation.stable_polls = observation.stable_polls.saturating_add(1);
            } else {
                observation.signature = signature;
                // The current scan is the first observation of the new
                // signature. Counting it keeps initial publication and a
                // later rewrite under the same deterministic rule.
                observation.stable_polls = 1;
            }
            let old_enough = signature
                .modified
                .and_then(|modified| now.duration_since(modified).ok())
                .is_none_or(|age| age >= self.min_age);
            if observation.stable_polls >= self.required_stable_polls
                && old_enough
                && observation.emitted != Some(signature)
            {
                observation.emitted = Some(signature);
                ready.push(path);
            }
        }
        Ok(ready)
    }

    /// Withdraw a publication after a higher-level NetCDF/readability check
    /// fails. The unchanged file must satisfy the full stability window again
    /// before it is returned, so a paused writer can resume safely.
    pub fn retry(&mut self, path: &Path) {
        if let Some(observation) = self.observations.get_mut(path) {
            observation.emitted = None;
            observation.stable_polls = 0;
        }
    }
}

fn collect_wrfout(
    directory: &Path,
    depth: usize,
    max_depth: usize,
    paths: &mut Vec<PathBuf>,
) -> Result<()> {
    if !directory.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_file()
            && entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("wrfout_d"))
        {
            paths.push(path);
        } else if file_type.is_dir() && depth < max_depth {
            collect_wrfout(&path, depth + 1, max_depth, paths)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn emits_only_stable_nonempty_wrfout_and_reemits_after_change() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("rw-sim-watch-{suffix}"));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("wrfout_d01_2026-07-18_00_00_00");
        fs::write(&path, b"first").unwrap();
        let mut watcher = StableWrfoutWatcher::new(2, Duration::ZERO);
        assert!(watcher.scan(&root).unwrap().is_empty());
        assert_eq!(watcher.scan(&root).unwrap(), vec![path.clone()]);
        assert!(watcher.scan(&root).unwrap().is_empty());
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b" second").unwrap();
        drop(file);
        assert!(watcher.scan(&root).unwrap().is_empty());
        assert_eq!(watcher.scan(&root).unwrap(), vec![path.clone()]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejected_publication_must_stabilize_again() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("rw-sim-watch-retry-{suffix}"));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("wrfout_d01_2026-07-18_00_00_00");
        fs::write(&path, b"not-netcdf-yet").unwrap();
        let mut watcher = StableWrfoutWatcher::new(2, Duration::ZERO);
        assert!(watcher.scan(&root).unwrap().is_empty());
        assert_eq!(watcher.scan(&root).unwrap(), vec![path.clone()]);
        watcher.retry(&path);
        assert!(watcher.scan(&root).unwrap().is_empty());
        assert_eq!(watcher.scan(&root).unwrap(), vec![path.clone()]);
        fs::remove_dir_all(root).unwrap();
    }
}
