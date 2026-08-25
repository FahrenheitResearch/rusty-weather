//! Auditable storm-model installation, selection, and canonical mask output.
//!
//! This crate deliberately has no network client, dynamic-library loader, UI,
//! or server integration. Model artifacts arrive only through an explicit
//! caller-controlled install, are identified by their exact SHA-256 bytes,
//! and are never executed as native code from disk. Native Rust models must be
//! compiled into a trusted application and explicitly registered at runtime.

mod mask;
mod native;
mod policy;
mod registry;

pub use mask::{
    GridGeometry, MaskOutput, ModelInputBatch, ModelInputPlane, OwnedMask,
    canonicalize_supplied_mask, validate_model_inputs,
};
pub use native::{NativeBackendRegistry, NativeStormModel};
pub use policy::{DistributionAudience, DistributionGrant, ModelUsePolicy};
pub use registry::{
    InstalledModel, ModelKey, ModelLimits, ModelRegistry, RegistryError, RegistryResult,
};
