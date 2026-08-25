use serde::{Deserialize, Serialize};

use crate::{RegistryError, RegistryResult};

/// The widest audience to which an artifact or its derived output may be sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DistributionGrant {
    /// No copying outside the node that holds the installed artifact.
    NodeOnly,
    /// May be sent to authenticated company users, never to a public client.
    CompanyInternal,
    /// May be published publicly, subject to the recorded attribution.
    Public,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DistributionAudience {
    NodeProcess,
    CompanyCoworker,
    PublicWebsite,
}

impl DistributionGrant {
    fn permits(self, audience: DistributionAudience) -> bool {
        matches!(audience, DistributionAudience::NodeProcess)
            || matches!(
                (self, audience),
                (
                    Self::CompanyInternal | Self::Public,
                    DistributionAudience::CompanyCoworker
                ) | (Self::Public, DistributionAudience::PublicWebsite)
            )
    }
}

/// Explicit rights attached to one immutable model artifact.
///
/// Artifact redistribution and publication of derived polygons are separate:
/// a private weight file can legally produce a public derived-data layer only
/// when the applicable license grants that right explicitly.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelUsePolicy {
    pub artifact_distribution: DistributionGrant,
    pub derived_output_distribution: DistributionGrant,
    /// Human-readable attribution that downstream publishers must retain.
    pub required_attribution: String,
    /// License, contract, or internal approval reference used for auditing.
    pub rights_reference: String,
}

impl ModelUsePolicy {
    pub fn private_company(
        required_attribution: impl Into<String>,
        rights_reference: impl Into<String>,
    ) -> Self {
        Self {
            artifact_distribution: DistributionGrant::NodeOnly,
            derived_output_distribution: DistributionGrant::CompanyInternal,
            required_attribution: required_attribution.into(),
            rights_reference: rights_reference.into(),
        }
    }

    pub fn validate(&self) -> RegistryResult<()> {
        validate_policy_text("required_attribution", &self.required_attribution, 1024)?;
        validate_policy_text("rights_reference", &self.rights_reference, 2048)
    }

    pub fn authorize_artifact(&self, audience: DistributionAudience) -> RegistryResult<()> {
        self.authorize("model artifact", self.artifact_distribution, audience)
    }

    pub fn authorize_derived_output(&self, audience: DistributionAudience) -> RegistryResult<()> {
        self.authorize(
            "model-derived storm output",
            self.derived_output_distribution,
            audience,
        )
    }

    fn authorize(
        &self,
        subject: &'static str,
        grant: DistributionGrant,
        audience: DistributionAudience,
    ) -> RegistryResult<()> {
        if grant.permits(audience) {
            Ok(())
        } else {
            Err(RegistryError::DistributionDenied { subject, audience })
        }
    }
}

fn validate_policy_text(field: &'static str, value: &str, limit: usize) -> RegistryResult<()> {
    if value.trim().is_empty() {
        return Err(RegistryError::InvalidPolicy {
            field,
            reason: "must not be blank",
        });
    }
    if value.len() > limit {
        return Err(RegistryError::InvalidPolicy {
            field,
            reason: "is too long",
        });
    }
    if value.chars().any(char::is_control) {
        return Err(RegistryError::InvalidPolicy {
            field,
            reason: "contains control characters",
        });
    }
    Ok(())
}
