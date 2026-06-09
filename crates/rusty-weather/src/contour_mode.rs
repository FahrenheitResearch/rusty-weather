use clap::ValueEnum;
use rustwx_products::derived::NativeContourRenderMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ContourModeArg {
    Automatic,
    Signature,
    LegacyRaster,
    ExperimentalAllProjected,
}

impl From<ContourModeArg> for NativeContourRenderMode {
    fn from(value: ContourModeArg) -> Self {
        match value {
            ContourModeArg::Automatic => Self::Automatic,
            ContourModeArg::Signature => Self::Signature,
            ContourModeArg::LegacyRaster => Self::LegacyRaster,
            ContourModeArg::ExperimentalAllProjected => Self::ExperimentalAllProjected,
        }
    }
}
