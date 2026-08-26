// SPDX-License-Identifier: Apache-2.0

use std::io;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum SimError {
    #[error(transparent)]
    Io(#[from] io::Error),
}

pub type Result<T> = std::result::Result<T, SimError>;
