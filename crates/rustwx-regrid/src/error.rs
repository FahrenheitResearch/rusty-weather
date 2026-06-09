use thiserror::Error;

#[derive(Debug, Error, PartialEq)]
pub enum RegridError {
    #[error("shape mismatch: expected {expected}, got {actual}")]
    ShapeMismatch { expected: usize, actual: usize },
    #[error("unsupported method '{method}' for geometry '{geometry}'")]
    UnsupportedMethodForGeometry { method: String, geometry: String },
    #[error("unsupported geometry: {0}")]
    UnsupportedGeometry(String),
    #[error("unsupported vector rotation: {0}")]
    UnsupportedVectorRotation(String),
    #[error("invalid grid: {0}")]
    InvalidGrid(String),
    #[error("invalid weights: {0}")]
    InvalidWeights(String),
    #[error("invalid options: {0}")]
    InvalidOptions(String),
    #[error("projection error: {0}")]
    Projection(String),
}
