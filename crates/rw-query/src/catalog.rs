use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use rw_store::run::validate_store_component;

use crate::snapshot::{DEFAULT_READER_POOL_BYTES, ReaderPool};
use crate::{
    ModelCatalogEntry, QueryError, QueryLimits, QueryResult, RunCatalogEntry, RunSnapshot,
};

#[derive(Debug, Clone)]
pub struct StoreCatalog {
    root: PathBuf,
    limits: QueryLimits,
    reader_pool: std::sync::Arc<ReaderPool>,
}

impl StoreCatalog {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self::with_limits(root, QueryLimits::default())
    }

    pub fn with_limits(root: impl AsRef<Path>, limits: QueryLimits) -> Self {
        Self::with_limits_and_reader_cache_bytes(root, limits, DEFAULT_READER_POOL_BYTES)
    }

    /// Build a catalog whose snapshots share a bounded pool of validated hour
    /// readers. The byte budget is reserved for decoded 2-D tile caches; mmap
    /// address space and readers still held by active queries are not counted.
    pub fn with_limits_and_reader_cache_bytes(
        root: impl AsRef<Path>,
        limits: QueryLimits,
        reader_cache_bytes: u64,
    ) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
            limits,
            reader_pool: std::sync::Arc::new(ReaderPool::new(reader_cache_bytes)),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Cheap readiness probe for the configured store root.
    ///
    /// This deliberately does not walk model/run trees: health checks may be
    /// frequent and unauthenticated, while full catalog validation belongs to
    /// the query endpoints. Reading at most one entry proves the real
    /// directory can currently be enumerated and preserves an empty store as
    /// a valid ready state.
    pub fn probe_readable(&self) -> QueryResult<()> {
        require_real_directory(&self.root, "store root")?;
        if let Some(entry) = fs::read_dir(&self.root)?.next() {
            entry?.file_type()?;
        }
        Ok(())
    }

    pub fn list_models(&self) -> QueryResult<Vec<ModelCatalogEntry>> {
        require_real_directory(&self.root, "store root")?;
        let mut models = Vec::new();
        let mut remaining = self.limits.max_catalog_entries;
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            consume_catalog_entry(&mut remaining, self.limits.max_catalog_entries)?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let model = utf8_name(&entry)?;
            validate_store_component("model", &model)?;
            if models.len() >= self.limits.max_catalog_entries {
                return Err(QueryError::LimitExceeded {
                    what: "catalog models",
                    requested: models.len() + 1,
                    limit: self.limits.max_catalog_entries,
                });
            }
            models
                .try_reserve(1)
                .map_err(|error| QueryError::Allocation {
                    what: "catalog model list",
                    detail: error.to_string(),
                })?;
            models.push(ModelCatalogEntry {
                run_count: self.count_runs(&entry.path(), &mut remaining)?,
                model,
            });
        }
        models.sort_by(|left, right| left.model.cmp(&right.model));
        Ok(models)
    }

    pub fn list_runs(&self, model: &str) -> QueryResult<Vec<RunCatalogEntry>> {
        validate_store_component("model", model)?;
        let model_dir = self.root.join(model);
        require_model_directory(&model_dir, model)?;
        let mut runs = Vec::new();
        let mut remaining = self.limits.max_catalog_entries;
        for entry in fs::read_dir(model_dir)? {
            let entry = entry?;
            consume_catalog_entry(&mut remaining, self.limits.max_catalog_entries)?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let run = utf8_name(&entry)?;
            validate_store_component("run", &run)?;
            let manifest_path = entry.path().join("run.json");
            match fs::symlink_metadata(&manifest_path) {
                Ok(metadata) if metadata.file_type().is_file() => {}
                Ok(_) => {
                    return Err(QueryError::InvalidRequest(format!(
                        "run manifest {} must be a regular file",
                        manifest_path.display()
                    )));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            }
            if runs.len() >= self.limits.max_catalog_entries {
                return Err(QueryError::LimitExceeded {
                    what: "catalog runs",
                    requested: runs.len() + 1,
                    limit: self.limits.max_catalog_entries,
                });
            }
            let snapshot = RunSnapshot::open_with_pool(
                &self.root,
                model,
                &run,
                self.limits.clone(),
                self.reader_pool.clone(),
            )?;
            let variable_count = snapshot
                .manifest()
                .hours
                .values()
                .flat_map(|hour| hour.variables.iter())
                .collect::<BTreeSet<_>>()
                .len();
            if variable_count > self.limits.max_catalog_entries {
                return Err(QueryError::LimitExceeded {
                    what: "catalog variables",
                    requested: variable_count,
                    limit: self.limits.max_catalog_entries,
                });
            }
            runs.try_reserve(1)
                .map_err(|error| QueryError::Allocation {
                    what: "catalog run list",
                    detail: error.to_string(),
                })?;
            runs.push(RunCatalogEntry {
                run: snapshot.descriptor().clone(),
                variable_count,
            });
        }
        runs.sort_by(|left, right| left.run.run.cmp(&right.run.run));
        Ok(runs)
    }

    pub fn snapshot(&self, model: &str, run: &str) -> QueryResult<RunSnapshot> {
        validate_store_component("model", model)?;
        validate_store_component("run", run)?;
        let model_dir = self.root.join(model);
        require_model_directory(&model_dir, model)?;
        require_run_directory(&model_dir.join(run), model, run)?;
        RunSnapshot::open_with_pool(
            &self.root,
            model,
            run,
            self.limits.clone(),
            self.reader_pool.clone(),
        )
    }

    fn count_runs(&self, model_dir: &Path, remaining: &mut usize) -> QueryResult<usize> {
        let mut count = 0usize;
        for entry in fs::read_dir(model_dir)? {
            let entry = entry?;
            consume_catalog_entry(remaining, self.limits.max_catalog_entries)?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let run = utf8_name(&entry)?;
            validate_store_component("run", &run)?;
            let manifest = entry.path().join("run.json");
            match fs::symlink_metadata(&manifest) {
                Ok(metadata) if metadata.file_type().is_file() => count += 1,
                Ok(_) => {
                    return Err(QueryError::InvalidRequest(format!(
                        "run manifest {} must be a regular file",
                        manifest.display()
                    )));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            if count > self.limits.max_catalog_entries {
                return Err(QueryError::LimitExceeded {
                    what: "catalog runs per model",
                    requested: count,
                    limit: self.limits.max_catalog_entries,
                });
            }
        }
        Ok(count)
    }
}

fn consume_catalog_entry(remaining: &mut usize, limit: usize) -> QueryResult<()> {
    if *remaining == 0 {
        return Err(QueryError::LimitExceeded {
            what: "catalog entries inspected",
            requested: limit.saturating_add(1),
            limit,
        });
    }
    *remaining -= 1;
    Ok(())
}

fn utf8_name(entry: &fs::DirEntry) -> QueryResult<String> {
    entry.file_name().into_string().map_err(|_| {
        QueryError::InvalidRequest(format!(
            "catalog entry {} is not valid UTF-8",
            entry.path().display()
        ))
    })
}

fn require_real_directory(path: &Path, label: &str) -> QueryResult<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() {
        return Err(QueryError::InvalidRequest(format!(
            "{label} path {} must be a real directory, not a symlink",
            path.display()
        )));
    }
    Ok(())
}

fn require_model_directory(path: &Path, model: &str) -> QueryResult<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(()),
        Ok(_) => Err(QueryError::InvalidRequest(format!(
            "model path {} must be a real directory, not a symlink",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(QueryError::UnknownModel(model.to_string()))
        }
        Err(error) => Err(error.into()),
    }
}

fn require_run_directory(path: &Path, model: &str, run: &str) -> QueryResult<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(()),
        Ok(_) => Err(QueryError::InvalidRequest(format!(
            "run path {} must be a real directory, not a symlink",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(QueryError::UnknownRun {
            model: model.to_string(),
            run: run.to_string(),
        }),
        Err(error) => Err(error.into()),
    }
}
