use crate::error::RegridError;
use crate::grid::{GridGeometry, VectorOrientation};
use crate::plan::RegridPlan;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VectorRegridPolicy {
    ComponentsAlreadyEarthRelative,
    SourceGridRelativeToEarthThenRegrid,
    RegridThenRotateToTargetGrid,
}

impl RegridPlan {
    pub fn apply_vector_f32(
        &self,
        source_u: &[f32],
        source_v: &[f32],
        policy: VectorRegridPolicy,
        source_grid: &dyn GridGeometry,
        target_grid: &dyn GridGeometry,
    ) -> Result<(Vec<f32>, Vec<f32>), RegridError> {
        if source_u.len() != source_v.len() {
            return Err(RegridError::ShapeMismatch {
                expected: source_u.len(),
                actual: source_v.len(),
            });
        }
        match policy {
            VectorRegridPolicy::ComponentsAlreadyEarthRelative => {
                Ok((self.apply_f32(source_u)?, self.apply_f32(source_v)?))
            }
            VectorRegridPolicy::SourceGridRelativeToEarthThenRegrid => {
                let angles = grid_relative_angles(source_grid, self.weights.source_len, "source")?;
                let (earth_u, earth_v) = rotate_grid_to_earth_f32(source_u, source_v, angles);
                Ok((self.apply_f32(&earth_u)?, self.apply_f32(&earth_v)?))
            }
            VectorRegridPolicy::RegridThenRotateToTargetGrid => {
                let angles = grid_relative_angles(target_grid, self.weights.target_len, "target")?;
                let earth_u = self.apply_f32(source_u)?;
                let earth_v = self.apply_f32(source_v)?;
                Ok(rotate_earth_to_grid_f32(&earth_u, &earth_v, angles))
            }
        }
    }

    pub fn apply_vector_f64(
        &self,
        source_u: &[f64],
        source_v: &[f64],
        policy: VectorRegridPolicy,
        source_grid: &dyn GridGeometry,
        target_grid: &dyn GridGeometry,
    ) -> Result<(Vec<f64>, Vec<f64>), RegridError> {
        if source_u.len() != source_v.len() {
            return Err(RegridError::ShapeMismatch {
                expected: source_u.len(),
                actual: source_v.len(),
            });
        }
        match policy {
            VectorRegridPolicy::ComponentsAlreadyEarthRelative => {
                Ok((self.apply_f64(source_u)?, self.apply_f64(source_v)?))
            }
            VectorRegridPolicy::SourceGridRelativeToEarthThenRegrid => {
                let angles = grid_relative_angles(source_grid, self.weights.source_len, "source")?;
                let (earth_u, earth_v) = rotate_grid_to_earth_f64(source_u, source_v, angles);
                Ok((self.apply_f64(&earth_u)?, self.apply_f64(&earth_v)?))
            }
            VectorRegridPolicy::RegridThenRotateToTargetGrid => {
                let angles = grid_relative_angles(target_grid, self.weights.target_len, "target")?;
                let earth_u = self.apply_f64(source_u)?;
                let earth_v = self.apply_f64(source_v)?;
                Ok(rotate_earth_to_grid_f64(&earth_u, &earth_v, angles))
            }
        }
    }
}

fn grid_relative_angles<'a>(
    grid: &'a dyn GridGeometry,
    expected_len: usize,
    role: &str,
) -> Result<&'a [f64], RegridError> {
    match grid.vector_orientation() {
        Some(VectorOrientation::GridRelative { angle_to_east_rad }) => {
            if angle_to_east_rad.len() != expected_len {
                return Err(RegridError::UnsupportedVectorRotation(format!(
                    "{role} grid-relative angle length {} does not match expected {expected_len}",
                    angle_to_east_rad.len()
                )));
            }
            if angle_to_east_rad.iter().any(|value| !value.is_finite()) {
                return Err(RegridError::UnsupportedVectorRotation(format!(
                    "{role} grid-relative angles must be finite"
                )));
            }
            Ok(angle_to_east_rad)
        }
        Some(VectorOrientation::EarthRelative) | None => {
            Err(RegridError::UnsupportedVectorRotation(format!(
                "{role} grid does not expose grid-relative angle_to_east_rad"
            )))
        }
    }
}

fn rotate_grid_to_earth_f32(u: &[f32], v: &[f32], angles: &[f64]) -> (Vec<f32>, Vec<f32>) {
    let mut earth_u = Vec::with_capacity(u.len());
    let mut earth_v = Vec::with_capacity(v.len());
    for ((&u, &v), &angle) in u.iter().zip(v).zip(angles) {
        let (sin, cos) = angle.sin_cos();
        earth_u.push(f64::from(u).mul_add(cos, -f64::from(v) * sin) as f32);
        earth_v.push(f64::from(u).mul_add(sin, f64::from(v) * cos) as f32);
    }
    (earth_u, earth_v)
}

fn rotate_grid_to_earth_f64(u: &[f64], v: &[f64], angles: &[f64]) -> (Vec<f64>, Vec<f64>) {
    let mut earth_u = Vec::with_capacity(u.len());
    let mut earth_v = Vec::with_capacity(v.len());
    for ((&u, &v), &angle) in u.iter().zip(v).zip(angles) {
        let (sin, cos) = angle.sin_cos();
        earth_u.push(u.mul_add(cos, -v * sin));
        earth_v.push(u.mul_add(sin, v * cos));
    }
    (earth_u, earth_v)
}

fn rotate_earth_to_grid_f32(u: &[f32], v: &[f32], angles: &[f64]) -> (Vec<f32>, Vec<f32>) {
    let mut grid_u = Vec::with_capacity(u.len());
    let mut grid_v = Vec::with_capacity(v.len());
    for ((&u, &v), &angle) in u.iter().zip(v).zip(angles) {
        let (sin, cos) = angle.sin_cos();
        grid_u.push(f64::from(u).mul_add(cos, f64::from(v) * sin) as f32);
        grid_v.push((-f64::from(u)).mul_add(sin, f64::from(v) * cos) as f32);
    }
    (grid_u, grid_v)
}

fn rotate_earth_to_grid_f64(u: &[f64], v: &[f64], angles: &[f64]) -> (Vec<f64>, Vec<f64>) {
    let mut grid_u = Vec::with_capacity(u.len());
    let mut grid_v = Vec::with_capacity(v.len());
    for ((&u, &v), &angle) in u.iter().zip(v).zip(angles) {
        let (sin, cos) = angle.sin_cos();
        grid_u.push(u.mul_add(cos, v * sin));
        grid_v.push((-u).mul_add(sin, v * cos));
    }
    (grid_u, grid_v)
}

#[cfg(test)]
mod tests {
    use rustwx_core::GridShape;

    use crate::{
        OrientedGrid, RegridMethod, RegridOptions, RegridPlan, RegularLatLonGrid,
        VectorOrientation, VectorRegridPolicy,
    };

    #[test]
    fn vector_earth_relative_matches_scalar_regridding() {
        let source =
            RegularLatLonGrid::new(GridShape::new(2, 2).unwrap(), 0.0, 0.0, 1.0, 1.0, false)
                .unwrap();
        let target = source.clone();
        let plan = RegridPlan::build(
            &source,
            &target,
            RegridOptions::new(RegridMethod::Nearest {
                max_distance_km: None,
            }),
        )
        .unwrap();
        let u = vec![1.0, 2.0, 3.0, 4.0];
        let v = vec![5.0, 6.0, 7.0, 8.0];
        let (out_u, out_v) = plan
            .apply_vector_f32(
                &u,
                &v,
                VectorRegridPolicy::ComponentsAlreadyEarthRelative,
                &source,
                &target,
            )
            .unwrap();
        assert_eq!(out_u, plan.apply_f32(&u).unwrap());
        assert_eq!(out_v, plan.apply_f32(&v).unwrap());
    }

    #[test]
    fn unsupported_vector_rotation_is_explicit() {
        let source =
            RegularLatLonGrid::new(GridShape::new(2, 2).unwrap(), 0.0, 0.0, 1.0, 1.0, false)
                .unwrap();
        let target = source.clone();
        let plan = RegridPlan::build(
            &source,
            &target,
            RegridOptions::new(RegridMethod::Nearest {
                max_distance_km: None,
            }),
        )
        .unwrap();
        let err = plan
            .apply_vector_f32(
                &[1.0; 4],
                &[1.0; 4],
                VectorRegridPolicy::SourceGridRelativeToEarthThenRegrid,
                &source,
                &target,
            )
            .unwrap_err();
        assert!(matches!(
            err,
            crate::RegridError::UnsupportedVectorRotation(_)
        ));
    }

    #[test]
    fn source_grid_relative_vectors_rotate_to_earth_before_regrid() {
        let source =
            RegularLatLonGrid::new(GridShape::new(1, 1).unwrap(), 0.0, 0.0, 1.0, 1.0, false)
                .unwrap();
        let oriented_source = OrientedGrid::new(
            source.clone(),
            VectorOrientation::GridRelative {
                angle_to_east_rad: vec![std::f64::consts::FRAC_PI_2],
            },
        );
        let plan = RegridPlan::build(
            &oriented_source,
            &source,
            RegridOptions::new(RegridMethod::Nearest {
                max_distance_km: None,
            }),
        )
        .unwrap();
        let (u, v) = plan
            .apply_vector_f64(
                &[1.0],
                &[0.0],
                VectorRegridPolicy::SourceGridRelativeToEarthThenRegrid,
                &oriented_source,
                &source,
            )
            .unwrap();
        assert!(u[0].abs() < 1.0e-12);
        assert!((v[0] - 1.0).abs() < 1.0e-12);
    }
}
