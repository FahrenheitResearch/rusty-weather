# rw-storm-ml

`rw-storm-ml` is the server- and UI-independent model boundary for storm-cell
segmentation. It preserves the same `StormCellFrame` geometry contract used by
the deterministic detector while keeping model installation and execution
explicit, auditable, and Rust-native.

## Safety and operational contract

- A version is installed from caller-supplied bytes only. This crate never
  downloads a model.
- `artifact_sha256` is verified while staging, on process restart, and before
  execution. A version directory is immutable; replacing the same model ID and
  version is rejected.
- The version directory becomes visible through a same-filesystem atomic
  rename. Enablement, the active version, and rollback history are stored in a
  separately atomically replaced state file.
- Model IDs and versions are portable single path components. Registry paths,
  artifacts, manifests, and policies may not be symlinks (Windows reparse
  points are rejected too).
- Every installed manifest must explicitly record a license and training
  provenance. A separate use policy controls artifact redistribution and
  derived-output publication; callers must identify the intended audience.
- Grid dimensions, point products, and aggregate label-contouring work use
  checked arithmetic before allocation. Their default policy has no arbitrary
  cardinality or resolution ceiling: actual address space/allocation capacity
  is authoritative, and the crate never silently resamples or truncates.
  Embedders may set smaller explicit `ModelLimits`; RW Server advertises the
  effective policy in its private storm-model catalog. Artifact/manifest byte
  limits remain explicit security boundaries, and the 64-plane input bound is
  the versioned executable wire contract rather than a spatial-data limit.
- A `native_rust` implementation is a trusted Rust value compiled into the
  application and registered through `NativeBackendRegistry`. Artifact files
  are never loaded as DLLs or executable native code.
- `tract_onnx` is a reserved manifest backend but intentionally has no executor
  in the default build. Adding a very large inference dependency before there
  is a selected production model would increase audit and binary cost. A future
  executor must be feature-gated, consume `open_verified_artifact()` directly,
  and must not add Python or a separate runtime.

## Adding a future compiled Rust model

1. Choose a permanent `model_id` and a never-reused `model_version`.
2. Produce an inert weights/config artifact and compute its exact lowercase
   SHA-256. Fill every `StormModelManifest` field, including input names,
   sources, fields, units, normalization bounds, producer, license, and
   training provenance.
3. Create a `ModelUsePolicy` from the actual rights. Do not grant public output
   merely because the artifact itself is private.
4. Call `ModelRegistry::install`. Review the installed record, then explicitly
   `enable` and `activate` it. Keep the previous enabled version so `rollback`
   remains immediate.
5. Implement `NativeStormModel` in Rust and register that compiled value under
   the exact `ModelKey`. The runtime checks the installed backend, input
   descriptors, common dimensions, enablement, digest, and publication policy
   before returning canonical geometry.
6. Add golden mask/geometry fixtures, restart and corruption tests, and an
   inference benchmark using production grid dimensions. Never fetch test
   weights during a build.

The operational shape is intentionally small and explicit:

```rust,no_run
use std::fs::File;
use rw_storm_ml::{ModelKey, ModelLimits, ModelRegistry, ModelUsePolicy};

# fn install(manifest: rw_ops_protocol::StormModelManifest) -> Result<(), Box<dyn std::error::Error>> {
let root = std::path::Path::new("/absolute/private/model-registry");
let mut registry = ModelRegistry::open(root, ModelLimits::default())?;
let key = ModelKey::new(&manifest.model_id, &manifest.model_version)?;
let policy = ModelUsePolicy::private_company(
    "Required model attribution",
    "example-license-record-2026-08",
);
registry.install(manifest, policy, File::open("model.weights")?)?;
registry.enable(&key)?;
registry.activate(&key)?;
# Ok(()) }
```

Production code should resolve `active_for_execution(model_id)` or an
explicit enabled key. It should not choose a version by sorting version text.
Activation history records the actual rollout order, which is what `rollback`
uses after a failed deployment or quality regression.

For a model hosted by a separate trusted process, install a `supplied_mask`
manifest and pass its row-major probability or integer-label mask to
`canonicalize_supplied_mask`. Zero is background for labels. Touching nonzero
labels remain distinct, and disconnected islands of one label become distinct
cells carrying the same `supplied_label` attribute.
