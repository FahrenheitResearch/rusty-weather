use rustwx_regrid::{
    GridShape, MissingPolicy, RegridMethod, RegridOptions, RegridPlan, RegularLatLonGrid,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let source = RegularLatLonGrid::new(GridShape::new(3, 3)?, 35.0, -100.0, 1.0, 1.0, false)?;
    let target = RegularLatLonGrid::new(GridShape::new(5, 5)?, 35.0, -100.0, 0.5, 0.5, false)?;

    let source_values = (0..source.shape.len())
        .map(|idx| {
            let y = idx / source.shape.nx;
            let x = idx % source.shape.nx;
            (source.lat_at_y(y) + 2.0 * source.lon_at_x(x)) as f32
        })
        .collect::<Vec<_>>();

    let plan = RegridPlan::build(
        &source,
        &target,
        RegridOptions {
            method: RegridMethod::Bilinear,
            missing_policy: MissingPolicy::RenormalizeValid,
            extrapolate: false,
        },
    )?;
    let target_values = plan.apply_f32(&source_values)?;

    println!("target shape: {}x{}", target.shape.nx, target.shape.ny);
    for (idx, value) in target_values.iter().take(8).enumerate() {
        println!("target[{idx}] = {value:.3}");
    }

    Ok(())
}
