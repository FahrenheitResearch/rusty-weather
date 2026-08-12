use wrf_formula::FormulaProvenance;

#[test]
fn formula_provenance_is_nameable_from_downstream_crates() {
    fn assert_public_type<T>() {}

    assert_public_type::<FormulaProvenance>();
}
