use crate::error::RegridError;

#[derive(Clone, Debug, PartialEq)]
pub struct SparseWeights {
    pub row_offsets: Vec<usize>,
    pub source_indices: Vec<usize>,
    pub weights: Vec<f64>,
    pub target_len: usize,
    pub source_len: usize,
}

impl SparseWeights {
    pub fn new(
        row_offsets: Vec<usize>,
        source_indices: Vec<usize>,
        weights: Vec<f64>,
        target_len: usize,
        source_len: usize,
    ) -> Result<Self, RegridError> {
        let weights = Self {
            row_offsets,
            source_indices,
            weights,
            target_len,
            source_len,
        };
        weights.validate()?;
        Ok(weights)
    }

    pub fn row(&self, target_index: usize) -> impl Iterator<Item = (usize, f64)> + '_ {
        let (start, end) =
            if target_index < self.target_len && target_index + 1 < self.row_offsets.len() {
                (
                    self.row_offsets[target_index],
                    self.row_offsets[target_index + 1],
                )
            } else {
                (0, 0)
            };
        self.source_indices[start..end]
            .iter()
            .copied()
            .zip(self.weights[start..end].iter().copied())
    }

    pub fn validate(&self) -> Result<(), RegridError> {
        if self.row_offsets.len() != self.target_len + 1 {
            return Err(RegridError::InvalidWeights(format!(
                "row_offsets length must be target_len + 1 ({}), got {}",
                self.target_len + 1,
                self.row_offsets.len()
            )));
        }
        if self.row_offsets.first().copied() != Some(0) {
            return Err(RegridError::InvalidWeights(
                "row_offsets must start at 0".to_string(),
            ));
        }
        if self.row_offsets.last().copied() != Some(self.source_indices.len()) {
            return Err(RegridError::InvalidWeights(format!(
                "last row offset must equal source_indices length {}, got {:?}",
                self.source_indices.len(),
                self.row_offsets.last()
            )));
        }
        if self.source_indices.len() != self.weights.len() {
            return Err(RegridError::InvalidWeights(format!(
                "source_indices length {} must equal weights length {}",
                self.source_indices.len(),
                self.weights.len()
            )));
        }
        let mut previous = 0;
        for (idx, &offset) in self.row_offsets.iter().enumerate() {
            if offset < previous {
                return Err(RegridError::InvalidWeights(format!(
                    "row offset {idx}={offset} is less than previous {previous}"
                )));
            }
            if offset > self.source_indices.len() {
                return Err(RegridError::InvalidWeights(format!(
                    "row offset {idx}={offset} exceeds source index length {}",
                    self.source_indices.len()
                )));
            }
            previous = offset;
        }
        for (idx, &source_index) in self.source_indices.iter().enumerate() {
            if source_index >= self.source_len {
                return Err(RegridError::InvalidWeights(format!(
                    "source index at weight {idx} is {source_index}, source_len is {}",
                    self.source_len
                )));
            }
        }
        for (idx, &weight) in self.weights.iter().enumerate() {
            if !weight.is_finite() {
                return Err(RegridError::InvalidWeights(format!(
                    "weight at index {idx} is not finite: {weight}"
                )));
            }
        }
        Ok(())
    }

    pub fn row_weight_sum(&self, target_index: usize) -> f64 {
        self.row(target_index).map(|(_, weight)| weight).sum()
    }
}

#[derive(Debug)]
pub(crate) struct SparseWeightBuilder {
    row_offsets: Vec<usize>,
    source_indices: Vec<usize>,
    weights: Vec<f64>,
    target_len: usize,
    source_len: usize,
}

impl SparseWeightBuilder {
    pub(crate) fn new(target_len: usize, source_len: usize) -> Self {
        Self {
            row_offsets: vec![0],
            source_indices: Vec::new(),
            weights: Vec::new(),
            target_len,
            source_len,
        }
    }

    pub(crate) fn push_row(&mut self, row: &[(usize, f64)]) {
        for &(source_index, weight) in row {
            if weight != 0.0 {
                self.source_indices.push(source_index);
                self.weights.push(weight);
            }
        }
        self.row_offsets.push(self.source_indices.len());
    }

    pub(crate) fn finish(self) -> Result<SparseWeights, RegridError> {
        SparseWeights::new(
            self.row_offsets,
            self.source_indices,
            self.weights,
            self.target_len,
            self.source_len,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::SparseWeights;

    #[test]
    fn sparse_weights_validation_accepts_valid_matrix() {
        let weights =
            SparseWeights::new(vec![0, 2, 2, 3], vec![0, 1, 1], vec![0.5, 0.5, 1.0], 3, 2);
        assert!(weights.is_ok());
    }

    #[test]
    fn sparse_weights_validation_rejects_bad_row_offsets() {
        let weights = SparseWeights::new(vec![0, 3, 2], vec![0, 1], vec![0.5, 0.5], 2, 2);
        assert!(weights.is_err());
    }

    #[test]
    fn sparse_weights_validation_rejects_out_of_bounds_source_index() {
        let weights = SparseWeights::new(vec![0, 1], vec![3], vec![1.0], 1, 2);
        assert!(weights.is_err());
    }

    #[test]
    fn sparse_weights_validation_rejects_nan_weight() {
        let weights = SparseWeights::new(vec![0, 1], vec![0], vec![f64::NAN], 1, 2);
        assert!(weights.is_err());
    }
}
