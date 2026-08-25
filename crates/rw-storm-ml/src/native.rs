use std::collections::BTreeMap;
use std::sync::Arc;

use rw_ops_protocol::{StormCellFrame, StormModelBackend};

use crate::mask::canonicalize_model_mask;
use crate::{
    DistributionAudience, ModelInputBatch, ModelKey, ModelLimits, ModelRegistry, OwnedMask,
    RegistryError, RegistryResult, validate_model_inputs,
};

/// Object-safe interface for a model compiled into the trusted Rust process.
///
/// Implementations are ordinary Rust values registered by application code.
/// The registry never loads a DLL, shared object, Wasm module, Python module,
/// or executable from an installed artifact.
pub trait NativeStormModel: Send + Sync {
    fn infer(&self, inputs: ModelInputBatch<'_>, limits: ModelLimits) -> RegistryResult<OwnedMask>;
}

#[derive(Default)]
pub struct NativeBackendRegistry {
    backends: BTreeMap<ModelKey, Arc<dyn NativeStormModel>>,
}

impl NativeBackendRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind a trusted compiled implementation to an immutable installed model
    /// identity. Registration is in-memory and must be repeated on restart.
    pub fn register(
        &mut self,
        models: &ModelRegistry,
        key: ModelKey,
        backend: Arc<dyn NativeStormModel>,
    ) -> RegistryResult<()> {
        let model = models.get(&key)?;
        if model.manifest.backend != StormModelBackend::NativeRust {
            return Err(RegistryError::BackendUnavailable(
                "only native_rust manifests accept compiled backend registration",
            ));
        }
        if self.backends.insert(key.clone(), backend).is_some() {
            return Err(RegistryError::NativeBackendDuplicate(key));
        }
        Ok(())
    }

    pub fn contains(&self, key: &ModelKey) -> bool {
        self.backends.contains_key(key)
    }

    pub fn infer(
        &self,
        models: &ModelRegistry,
        key: &ModelKey,
        inputs: ModelInputBatch<'_>,
        audience: DistributionAudience,
    ) -> RegistryResult<OwnedMask> {
        let model = models.enabled_for_execution(key)?;
        model.authorize_derived_output(audience)?;
        if model.manifest.backend != StormModelBackend::NativeRust {
            return Err(RegistryError::BackendUnavailable(
                match model.manifest.backend {
                    StormModelBackend::TractOnnx => {
                        "tract_onnx support is not compiled; no artifact is executed implicitly"
                    }
                    StormModelBackend::SuppliedMask => {
                        "supplied_mask models accept an explicit output mask, not native inference"
                    }
                    StormModelBackend::NativeRust => unreachable!(),
                },
            ));
        }
        validate_model_inputs(model, inputs, models.limits())?;
        let backend = self
            .backends
            .get(key)
            .ok_or_else(|| RegistryError::NativeBackendMissing(key.clone()))?;
        let output = backend.infer(inputs, models.limits())?;
        validate_owned_mask(
            output.as_output(),
            inputs.geometry.shape(models.limits())?,
            models.limits(),
        )?;
        Ok(output)
    }

    pub fn infer_canonical(
        &self,
        models: &ModelRegistry,
        key: &ModelKey,
        generated_at_unix_ms: i64,
        inputs: ModelInputBatch<'_>,
        audience: DistributionAudience,
    ) -> RegistryResult<StormCellFrame> {
        let output = self.infer(models, key, inputs, audience)?;
        let model = models.enabled_for_execution(key)?;
        canonicalize_model_mask(
            model,
            inputs.source.clone(),
            generated_at_unix_ms,
            inputs.geometry,
            output.as_output(),
            audience,
            models.limits(),
        )
    }
}

fn validate_owned_mask(
    mask: crate::MaskOutput<'_>,
    expected: (usize, usize),
    _limits: ModelLimits,
) -> RegistryResult<()> {
    let (width, height, values) = match mask {
        crate::MaskOutput::Probabilities {
            width,
            height,
            values,
        } => {
            if let Some((index, value)) = values
                .iter()
                .copied()
                .enumerate()
                .find(|(_, value)| value.is_finite() && !(0.0..=1.0).contains(value))
            {
                return Err(RegistryError::InvalidOutput(format!(
                    "native probability {value} at index {index} is outside [0, 1]"
                )));
            }
            (width, height, values.len())
        }
        crate::MaskOutput::Labels {
            width,
            height,
            values,
        } => (width, height, values.len()),
    };
    let expected_values = expected
        .0
        .checked_mul(expected.1)
        .ok_or_else(|| RegistryError::InvalidOutput("grid size overflow".into()))?;
    if (width, height) != expected || values != expected_values {
        return Err(RegistryError::InvalidOutput(format!(
            "native backend returned {width}x{height}/{values} values; expected {}x{}/{expected_values}",
            expected.0, expected.1
        )));
    }
    Ok(())
}
