#[derive(Clone, Debug, PartialEq)]
pub enum RegridMethod {
    Nearest {
        max_distance_km: Option<f64>,
    },
    Bilinear,
    InverseDistance {
        k: usize,
        power: f64,
        radius_km: Option<f64>,
    },
    Conservative {
        normalization: ConservativeNormalization,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConservativeNormalization {
    TargetArea,
    CoveredArea,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MissingPolicy {
    Propagate,
    RenormalizeValid,
    FillValueF32(f32),
    FillValueF64(f64),
}

#[derive(Clone, Debug, PartialEq)]
pub struct RegridOptions {
    pub method: RegridMethod,
    pub missing_policy: MissingPolicy,
    pub extrapolate: bool,
}

impl RegridOptions {
    pub fn new(method: RegridMethod) -> Self {
        Self {
            method,
            missing_policy: MissingPolicy::Propagate,
            extrapolate: false,
        }
    }
}
