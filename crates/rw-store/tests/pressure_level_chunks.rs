use std::fs;
use std::path::{Path, PathBuf};

use rw_store::error::RwStoreError;
use rw_store::format::{COL_X, COL_Y};
use rw_store::reader::{
    HourReader, PressureLevelChunkData3D, SelectedPressureLevelChunkData3D,
    SelectedPressureLevelPlane3D,
};
use rw_store::writer::HourWriter;

const NX: usize = 19;
const NY: usize = 18;
const LEVELS: [u16; 3] = [1000, 850, 500];

fn test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rw-store-pressure-level-chunks-{}-{name}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_sample(path: &Path) {
    let mut planes: Vec<Vec<f32>> = LEVELS
        .iter()
        .enumerate()
        .map(|(level_index, _)| {
            (0..NY)
                .flat_map(|y| {
                    (0..NX).map(move |x| level_index as f32 * 100.0 + y as f32 * 2.0 + x as f32)
                })
                .collect()
        })
        .collect();

    for plane in &mut planes {
        // Chunk (0,0) is payload-free EMPTY at every pressure level.
        for y in 0..COL_Y {
            for x in 0..COL_X {
                plane[y * NX + x] = f32::NAN;
            }
        }
        // Right-edge chunk (0,1) is payload-free CONSTANT across all levels.
        for y in 0..COL_Y {
            for x in COL_X..NX {
                plane[y * NX + x] = 42.0;
            }
        }
    }
    // Keep chunk (1,0) dense while exercising an inline missing value on the
    // requested level. Chunk (1,1) remains a dense bottom-right edge chunk.
    planes[1][17 * NX + 1] = f32::NAN;

    let plane_refs: Vec<&[f32]> = planes.iter().map(Vec::as_slice).collect();
    let mut writer = HourWriter::new("hrrr", "20260811_00z", 0, NX, NY, "grid", "test");
    writer
        .add_pressure3d(
            "temperature",
            "K",
            serde_json::Value::Null,
            &LEVELS,
            &plane_refs,
        )
        .unwrap();
    writer
        .add_surface2d(
            "temperature_2m",
            "K",
            serde_json::Value::Null,
            &vec![280.0; NX * NY],
        )
        .unwrap();
    writer.finish(path).unwrap();
}

fn assert_bits_eq(actual: &[f32], expected: &[f32]) {
    assert_eq!(actual.len(), expected.len());
    for (index, (actual, expected)) in actual.iter().zip(expected).enumerate() {
        assert_eq!(
            actual.to_bits(),
            expected.to_bits(),
            "f32 bits differ at plane index {index}: {actual:?} versus {expected:?}"
        );
    }
}

#[test]
fn enumerates_edge_geometry_and_reads_storage_specializations() {
    let dir = test_dir("geometry");
    let path = dir.join("hour.rws");
    write_sample(&path);
    let reader = HourReader::open(&path).unwrap();

    let chunks = reader.pressure_level_chunks_3d("temperature", 850).unwrap();
    assert_eq!(
        (
            chunks.chunks_y(),
            chunks.chunks_x(),
            chunks.level_hpa(),
            chunks.len(),
        ),
        (2, 2, 850, 4)
    );
    let geometries: Vec<_> = chunks.collect();
    assert_eq!(
        geometries
            .iter()
            .map(|geometry| (
                geometry.chunk_y(),
                geometry.chunk_x(),
                geometry.y0(),
                geometry.x0(),
                geometry.height(),
                geometry.width(),
                geometry.level_hpa(),
            ))
            .collect::<Vec<_>>(),
        vec![
            (0, 0, 0, 0, 16, 16, 850),
            (0, 1, 0, 16, 16, 3, 850),
            (1, 0, 16, 0, 2, 16, 850),
            (1, 1, 16, 16, 2, 3, 850),
        ]
    );

    let empty = reader
        .read_pressure_level_chunk_3d("temperature", 850, 0, 0)
        .unwrap();
    assert!(matches!(empty.data(), PressureLevelChunkData3D::Empty));
    assert!(empty.get(0, 0).unwrap().is_nan());
    assert!(empty.get(COL_Y, 0).is_none());

    let constant = reader
        .read_pressure_level_chunk_3d("temperature", 850, 0, 1)
        .unwrap();
    match constant.data() {
        PressureLevelChunkData3D::Constant(value) => {
            assert_eq!(value.to_bits(), 42.0f32.to_bits())
        }
        other => panic!("expected constant chunk plane, got {other:?}"),
    }
    assert_eq!(constant.get(15, 2).unwrap().to_bits(), 42.0f32.to_bits());
    assert!(constant.get(0, 3).is_none());

    let dense_missing = reader
        .read_pressure_level_chunk_3d("temperature", 850, 1, 0)
        .unwrap();
    match dense_missing.data() {
        PressureLevelChunkData3D::Dense(values) => {
            assert_eq!(values.len(), 2 * 16);
            assert!(dense_missing.get(1, 1).unwrap().is_nan());
        }
        other => panic!("expected dense chunk plane, got {other:?}"),
    }

    let dense_edge = reader
        .read_pressure_level_chunk_3d("temperature", 850, 1, 1)
        .unwrap();
    assert_eq!(
        (
            dense_edge.geometry().x0(),
            dense_edge.geometry().y0(),
            dense_edge.geometry().width(),
            dense_edge.geometry().height(),
            dense_edge.cell_count(),
        ),
        (16, 16, 3, 2, 6)
    );
    assert!(matches!(
        dense_edge.data(),
        PressureLevelChunkData3D::Dense(values) if values.len() == 6
    ));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn chunk_visitor_is_bit_exact_with_read_level_and_stops_on_error() {
    let dir = test_dir("parity");
    let path = dir.join("hour.rws");
    write_sample(&path);
    let reader = HourReader::open(&path).unwrap();

    let expected = reader.read_level_3d("temperature", 850).unwrap();
    let mut assembled = vec![0.0; NX * NY];
    let mut visits = 0usize;
    let mut largest_plane = 0usize;
    reader
        .visit_pressure_level_chunks_3d("temperature", 850, |chunk| {
            visits += 1;
            largest_plane = largest_plane.max(chunk.cell_count());
            let geometry = chunk.geometry();
            for row in 0..geometry.height() {
                for column in 0..geometry.width() {
                    assembled[(geometry.y0() + row) * NX + geometry.x0() + column] =
                        chunk.get(row, column).unwrap();
                }
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(visits, 4);
    assert_eq!(largest_plane, COL_Y * COL_X);
    assert_bits_eq(&assembled, &expected);

    let mut stopped_after = 0usize;
    let error = reader
        .visit_pressure_level_chunks_3d("temperature", 850, |_| {
            stopped_after += 1;
            Err(RwStoreError::Chunk("cancelled by caller".to_string()))
        })
        .unwrap_err();
    assert_eq!(stopped_after, 1);
    assert!(
        matches!(&error, RwStoreError::Chunk(message) if message == "cancelled by caller"),
        "unexpected visitor error: {error:?}"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn rejects_unknown_variable_wrong_kind_level_and_chunk_bounds() {
    let dir = test_dir("errors");
    let path = dir.join("hour.rws");
    write_sample(&path);
    let reader = HourReader::open(&path).unwrap();

    let unknown = reader
        .pressure_level_chunks_3d("not_present", 850)
        .unwrap_err();
    assert!(
        matches!(&unknown, RwStoreError::UnknownVariable(name) if name == "not_present"),
        "unexpected unknown-variable error: {unknown:?}"
    );

    for error in [
        reader
            .pressure_level_chunks_3d("temperature_2m", 850)
            .unwrap_err(),
        reader
            .read_pressure_level_chunk_3d("temperature_2m", 850, 0, 0)
            .unwrap_err(),
        reader
            .visit_pressure_level_chunks_3d("temperature_2m", 850, |_| Ok(()))
            .unwrap_err(),
    ] {
        match error {
            RwStoreError::Format(message) => assert!(
                message.contains("surface2d") && message.contains("pressure3d"),
                "unexpected wrong-kind message: {message}"
            ),
            other => panic!("expected Format for wrong kind, got {other:?}"),
        }
    }

    for error in [
        reader
            .pressure_level_chunks_3d("temperature", 700)
            .unwrap_err(),
        reader
            .read_pressure_level_chunk_3d("temperature", 700, 0, 0)
            .unwrap_err(),
        reader
            .visit_pressure_level_chunks_3d("temperature", 700, |_| Ok(()))
            .unwrap_err(),
    ] {
        match error {
            RwStoreError::Meta(message) => assert!(
                message.contains("no 700 hPa level"),
                "unexpected missing-level message: {message}"
            ),
            other => panic!("expected Meta for missing level, got {other:?}"),
        }
    }

    for (chunk_y, chunk_x) in [(2, 0), (0, 2), (usize::MAX, usize::MAX)] {
        let error = reader
            .read_pressure_level_chunk_3d("temperature", 850, chunk_y, chunk_x)
            .unwrap_err();
        match error {
            RwStoreError::Format(message) => assert!(
                message.contains("outside chunk grid 2x2"),
                "unexpected chunk-bounds message: {message}"
            ),
            other => panic!("expected Format for invalid chunk, got {other:?}"),
        }
    }

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn selected_levels_preserve_request_order_and_match_single_level_bits() {
    let dir = test_dir("selected-order-parity");
    let path = dir.join("hour.rws");
    write_sample(&path);
    let reader = HourReader::open(&path).unwrap();

    let requested = [500, 1000, 850];
    let chunks = reader
        .selected_pressure_level_chunks_3d("temperature", &requested)
        .unwrap();
    assert_eq!(chunks.levels_hpa(), requested);
    assert_eq!(
        (chunks.chunks_y(), chunks.chunks_x(), chunks.len()),
        (2, 2, 4)
    );

    let expected: Vec<Vec<f32>> = requested
        .iter()
        .map(|level| reader.read_level_3d("temperature", *level).unwrap())
        .collect();
    let mut assembled = vec![vec![0.0; NX * NY]; requested.len()];
    let mut visits = 0usize;
    let mut largest_dense_output = 0usize;
    reader
        .visit_selected_pressure_level_chunks_3d("temperature", &requested, |chunk| {
            visits += 1;
            assert_eq!(chunk.levels_hpa(), requested);
            assert_eq!(chunk.value_count(), chunk.cell_count() * requested.len());
            if let SelectedPressureLevelChunkData3D::Dense(values) = chunk.data() {
                largest_dense_output = largest_dense_output.max(values.len());
                assert_eq!(values.len(), chunk.cell_count() * requested.len());
            }
            let geometry = chunk.geometry();
            for selected_level_index in 0..requested.len() {
                for row in 0..geometry.height() {
                    for column in 0..geometry.width() {
                        assembled[selected_level_index]
                            [(geometry.y0() + row) * NX + geometry.x0() + column] =
                            chunk.get(selected_level_index, row, column).unwrap();
                    }
                }
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(visits, 4);
    assert!(largest_dense_output <= requested.len() * COL_Y * COL_X);
    for (actual, expected) in assembled.iter().zip(&expected) {
        assert_bits_eq(actual, expected);
    }

    let dense_edge = reader
        .read_selected_pressure_level_chunk_3d("temperature", &requested, 1, 1)
        .unwrap();
    assert_eq!(
        (
            dense_edge.geometry().x0(),
            dense_edge.geometry().y0(),
            dense_edge.geometry().width(),
            dense_edge.geometry().height(),
            dense_edge.cell_count(),
            dense_edge.value_count(),
        ),
        (16, 16, 3, 2, 6, 18)
    );
    for selected_level_index in 0..requested.len() {
        match dense_edge.plane(selected_level_index).unwrap() {
            SelectedPressureLevelPlane3D::Dense(values) => assert_eq!(values.len(), 6),
            other => panic!("expected dense selected plane, got {other:?}"),
        }
    }
    assert!(dense_edge.plane(requested.len()).is_none());
    assert!(dense_edge.get(0, 2, 0).is_none());
    assert!(dense_edge.get(requested.len(), 0, 0).is_none());

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn selected_levels_keep_empty_and_constant_chunks_allocation_free() {
    let dir = test_dir("selected-specializations");
    let path = dir.join("hour.rws");
    write_sample(&path);
    let reader = HourReader::open(&path).unwrap();
    let requested = [850, 500];

    let empty = reader
        .read_selected_pressure_level_chunk_3d("temperature", &requested, 0, 0)
        .unwrap();
    assert!(matches!(
        empty.data(),
        SelectedPressureLevelChunkData3D::Empty
    ));
    assert!(matches!(
        empty.plane(1),
        Some(SelectedPressureLevelPlane3D::Empty)
    ));
    assert!(empty.get(0, 0, 0).unwrap().is_nan());

    let constant = reader
        .read_selected_pressure_level_chunk_3d("temperature", &requested, 0, 1)
        .unwrap();
    match constant.data() {
        SelectedPressureLevelChunkData3D::Constant(value) => {
            assert_eq!(value.to_bits(), 42.0f32.to_bits())
        }
        other => panic!("expected allocation-free constant chunk, got {other:?}"),
    }
    assert!(matches!(
        constant.plane(0),
        Some(SelectedPressureLevelPlane3D::Constant(value))
            if value.to_bits() == 42.0f32.to_bits()
    ));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn selected_levels_reject_empty_duplicate_missing_and_stop_early() {
    let dir = test_dir("selected-errors-cancel");
    let path = dir.join("hour.rws");
    write_sample(&path);
    let reader = HourReader::open(&path).unwrap();

    for error in [
        reader
            .selected_pressure_level_chunks_3d("temperature", &[])
            .unwrap_err(),
        reader
            .read_selected_pressure_level_chunk_3d("temperature", &[], 0, 0)
            .unwrap_err(),
        reader
            .visit_selected_pressure_level_chunks_3d("temperature", &[], |_| Ok(()))
            .unwrap_err(),
    ] {
        assert!(
            matches!(&error, RwStoreError::Format(message) if message.contains("must not be empty")),
            "unexpected empty-selection error: {error:?}"
        );
    }

    for error in [
        reader
            .selected_pressure_level_chunks_3d("temperature", &[850, 850])
            .unwrap_err(),
        reader
            .read_selected_pressure_level_chunk_3d("temperature", &[850, 850], 0, 0)
            .unwrap_err(),
        reader
            .visit_selected_pressure_level_chunks_3d("temperature", &[850, 850], |_| Ok(()))
            .unwrap_err(),
    ] {
        assert!(
            matches!(&error, RwStoreError::Format(message) if message.contains("850 hPa") && message.contains("duplicated")),
            "unexpected duplicate-selection error: {error:?}"
        );
    }

    for error in [
        reader
            .selected_pressure_level_chunks_3d("temperature", &[850, 700])
            .unwrap_err(),
        reader
            .read_selected_pressure_level_chunk_3d("temperature", &[850, 700], 0, 0)
            .unwrap_err(),
        reader
            .visit_selected_pressure_level_chunks_3d("temperature", &[850, 700], |_| Ok(()))
            .unwrap_err(),
    ] {
        assert!(
            matches!(&error, RwStoreError::Meta(message) if message.contains("no 700 hPa level")),
            "unexpected missing-selection error: {error:?}"
        );
    }

    let mut stopped_after = 0usize;
    let error = reader
        .visit_selected_pressure_level_chunks_3d("temperature", &[500, 850], |_| {
            stopped_after += 1;
            Err(RwStoreError::Chunk("cancel selected visitor".to_string()))
        })
        .unwrap_err();
    assert_eq!(stopped_after, 1);
    assert!(
        matches!(&error, RwStoreError::Chunk(message) if message == "cancel selected visitor"),
        "unexpected cancellation error: {error:?}"
    );

    for (chunk_y, chunk_x) in [(2, 0), (0, 2), (usize::MAX, usize::MAX)] {
        let error = reader
            .read_selected_pressure_level_chunk_3d("temperature", &[500, 850], chunk_y, chunk_x)
            .unwrap_err();
        assert!(
            matches!(&error, RwStoreError::Format(message) if message.contains("outside chunk grid 2x2")),
            "unexpected selected chunk-bounds error: {error:?}"
        );
    }

    let _ = fs::remove_dir_all(&dir);
}
