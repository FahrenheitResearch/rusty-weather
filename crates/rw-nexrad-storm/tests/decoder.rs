use std::io::Write;

use bzip2::Compression as BzipCompression;
use bzip2::write::BzEncoder;
use rw_nexrad_storm::{
    Compression, DecodeLimits, DecodeOptions, DerivedGeometryProvenance, HeightQualifier,
    Level2DerivedGeometryRef, NexradStormProduct, PairingOptions, SiteIdentitySource, StormMotion,
    SuppliedGeometry, ValidationNotice, decode, decode_with_options, pair_geometry,
};

const WMO_STI: &[u8] = b"SDUS34 KOUN 222320\r\r\nNSTTLX\r\r\n";
const WMO_SS: &[u8] = b"SDUS64 KOUN 222320\r\r\nNSSTLX\r\r\n";

#[test]
fn decodes_tracking_points_tracks_motion_and_provenance() {
    let bytes = tracking_fixture();
    let NexradStormProduct::StormTracking(product) = decode(&bytes).expect("tracking product")
    else {
        panic!("wrong product variant");
    };
    assert_eq!(product.identity.message_code, 58);
    assert_eq!(product.identity.radar_site.site_id.as_deref(), Some("TLX"));
    assert_eq!(
        product.identity.radar_site.source,
        SiteIdentitySource::WmoProductIdentifier
    );
    assert_eq!(product.identity.compression, Compression::None);
    assert_eq!(
        product.identity.provenance.supplied_geometry,
        SuppliedGeometry::CentroidPointsAndTracks
    );
    assert!(
        product
            .identity
            .provenance
            .geometry_statement
            .contains("does not supply storm polygons")
    );
    assert_eq!(product.forecast_interval_minutes, Some(15));
    assert_eq!(product.number_of_past_volumes, Some(10));
    assert_eq!(product.cells.len(), 1);
    let cell = &product.cells[0];
    assert_eq!(cell.storm_id, "A1");
    assert_eq!(cell.current.position.i_quarter_km, 0);
    assert_eq!(cell.current.position.j_quarter_km, 741);
    assert_eq!(cell.history_in_packet_order.len(), 1);
    assert_eq!(cell.forecasts.len(), 1);
    assert_eq!(cell.forecasts[0].lead_minutes, Some(15));
    assert_eq!(
        cell.forecasts[0].tabular_position.unwrap().azimuth_degrees,
        3
    );
    assert_eq!(
        cell.motion,
        StormMotion::Moving {
            direction_from_degrees: 270,
            speed_knots: 20
        }
    );
    assert_eq!(cell.forecast_error_nautical_miles, Some(1.5));
    assert!(cell.current.valid_at_unix_ms.is_some());
    assert_eq!(cell.history_in_packet_order[0].valid_at_unix_ms, None);
}

#[test]
fn caller_site_hint_preserves_the_exact_four_character_identity() {
    let bytes = tracking_fixture();
    let options = DecodeOptions {
        site_hint: Some("KTLX".to_owned()),
        ..DecodeOptions::default()
    };
    let product = decode_with_options(&bytes, &options).expect("hinted product");
    assert_eq!(
        product.identity().radar_site.site_id.as_deref(),
        Some("KTLX")
    );
    assert_eq!(
        product.identity().radar_site.source,
        SiteIdentitySource::CallerHint
    );
}

#[test]
fn decodes_bzip2_body_with_an_exact_size_contract() {
    let fixture = tracking_fixture();
    let binary = &fixture[WMO_STI.len()..];
    let body = &binary[120..];
    let mut encoder = BzEncoder::new(Vec::new(), BzipCompression::best());
    encoder.write_all(body).expect("compress body");
    let compressed = encoder.finish().expect("finish bzip2");
    let mut message = binary[..120].to_vec();
    put_u16(&mut message, 100, 1);
    put_u32(&mut message, 102, u32::try_from(body.len()).unwrap());
    message.extend_from_slice(&compressed);
    let message_len = u32::try_from(message.len()).unwrap();
    put_u32(&mut message, 8, message_len);
    let mut input = WMO_STI.to_vec();
    input.extend(message);
    let product = decode(&input).expect("compressed product");
    assert_eq!(product.identity().compression, Compression::Bzip2);
}

#[test]
fn decodes_storm_structure_format_v_attributes() {
    let bytes = structure_fixture();
    let NexradStormProduct::StormStructure(product) = decode(&bytes).expect("structure product")
    else {
        panic!("wrong product variant");
    };
    assert_eq!(product.reported_cell_count, Some(1));
    assert_eq!(product.cells.len(), 1);
    let cell = &product.cells[0];
    assert_eq!(cell.storm_id, "A1");
    assert_eq!(cell.position.azimuth_degrees, 247);
    assert_eq!(cell.position.range_nautical_miles, 85);
    assert_eq!(
        cell.base_kft_agl.qualifier,
        HeightQualifier::BelowLowestElevation
    );
    assert_eq!(cell.base_kft_agl.kft_agl, 9.5);
    assert_eq!(cell.top_kft_agl.kft_agl, 40.2);
    assert_eq!(cell.cell_based_vil_kg_m2, 65);
    assert_eq!(cell.maximum_reflectivity_dbz, 61);
    assert_eq!(cell.maximum_reflectivity_height_kft_agl, 26.8);
    assert_eq!(
        product.identity.provenance.supplied_geometry,
        SuppliedGeometry::CentroidPointsOnly
    );
}

#[test]
fn zero_cell_structure_can_disclose_a_stale_optional_trend_offset() {
    let mut bytes = structure_fixture_with_rows(&[], 0);
    let start = WMO_SS.len();
    put_u32(&mut bytes, start + 112, 30_000);
    let NexradStormProduct::StormStructure(product) = decode(&bytes).expect("zero-cell table")
    else {
        panic!("wrong product variant");
    };
    assert!(product.cells.is_empty());
    assert!(matches!(
        product.identity.validation_notices.as_slice(),
        [ValidationNotice::IgnoredOutOfRangeOptionalCellTrendOffset { .. }]
    ));
}

#[test]
fn every_truncated_prefix_is_an_error_and_never_panics() {
    for fixture in [tracking_fixture(), structure_fixture()] {
        for end in 0..fixture.len() {
            let result = std::panic::catch_unwind(|| decode(&fixture[..end]));
            assert!(result.is_ok(), "panic at prefix {end}");
            assert!(result.unwrap().is_err(), "accepted truncated prefix {end}");
        }
    }
}

#[test]
fn malformed_offsets_packet_lengths_and_table_lengths_are_rejected() {
    let mut bad_offset = tracking_fixture();
    put_u32(&mut bad_offset, WMO_STI.len() + 108, u32::MAX);
    assert!(decode(&bad_offset).is_err());

    let mut bad_packet = tracking_fixture();
    // Binary symbology begins at 120; block+layer headers consume 16 bytes.
    put_u16(&mut bad_packet, WMO_STI.len() + 120 + 16 + 2, u16::MAX);
    assert!(decode(&bad_packet).is_err());

    let mut bad_page_count = tracking_fixture();
    let binary_start = WMO_STI.len();
    let table_halfwords = read_u32(&bad_page_count, binary_start + 116);
    let table = binary_start + usize::try_from(table_halfwords).unwrap() * 2;
    put_u16(&mut bad_page_count, table + 8 + 120 + 2, 49);
    assert!(decode(&bad_page_count).is_err());

    let mut bad_line = tracking_fixture();
    put_u16(&mut bad_line, table + 8 + 120 + 4, 81);
    assert!(decode(&bad_line).is_err());
}

#[test]
fn deterministic_byte_mutations_never_unwind() {
    let original = tracking_fixture();
    let mut state = 0x9e37_79b9_u32;
    for _ in 0..2_000 {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let mut mutated = original.clone();
        let index = usize::try_from(state).unwrap_or_default() % mutated.len();
        let shift = u8::try_from((state >> 29) & 7).unwrap_or_default();
        mutated[index] ^= 1_u8 << shift;
        assert!(std::panic::catch_unwind(|| decode(&mutated)).is_ok());
    }
}

#[test]
fn configured_resource_limits_are_enforced_before_decoding() {
    let fixture = tracking_fixture();
    let options = DecodeOptions {
        site_hint: None,
        limits: DecodeLimits {
            max_input_bytes: fixture.len() - 1,
            ..DecodeLimits::default()
        },
    };
    assert!(decode_with_options(&fixture, &options).is_err());
}

#[test]
fn pairing_keeps_authoritative_identity_and_derived_geometry_provenance_separate() {
    let NexradStormProduct::StormTracking(tracking) = decode(&tracking_fixture()).unwrap() else {
        panic!("wrong product variant");
    };
    let centroid = tracking.cells[0].current.geographic;
    let geometry = Level2DerivedGeometryRef {
        geometry_id: "l2-cell-7".to_owned(),
        site_id: "KTLX".to_owned(),
        volume_scan_at_unix_ms: tracking.identity.volume_scan_at_unix_ms + 30_000,
        centroid,
        provenance: DerivedGeometryProvenance {
            source_kind: "nexrad_level_ii".to_owned(),
            source_id: "KTLX-20260822T232010Z".to_owned(),
            method_id: "rw_contour_cell".to_owned(),
            method_version: "1".to_owned(),
            moment: "REF".to_owned(),
        },
    };
    let result = pair_geometry(
        &tracking,
        std::slice::from_ref(&geometry),
        PairingOptions::default(),
    )
    .expect("pairing");
    assert_eq!(result.associations.len(), 1);
    let association = &result.associations[0];
    assert_eq!(association.storm_id, "A1");
    assert_eq!(association.derived_geometry.provenance, geometry.provenance);
    assert!(
        association
            .provenance_statement
            .contains("not a NOAA/RPG polygon")
    );
    assert_eq!(association.tracking_product.message_code, 58);

    let mut wrong_site = geometry;
    wrong_site.site_id = "VNX".to_owned();
    let unpaired = pair_geometry(&tracking, &[wrong_site], PairingOptions::default()).unwrap();
    assert!(unpaired.associations.is_empty());
    assert_eq!(unpaired.unmatched_storm_ids, ["A1"]);
}

fn tracking_fixture() -> Vec<u8> {
    let mut sym_packets = Vec::new();
    sym_packets.extend(packet(2, &special_data(0, 741, 0x22)));
    let mut id = Vec::new();
    id.extend_from_slice(&0_i16.to_be_bytes());
    id.extend_from_slice(&741_i16.to_be_bytes());
    id.extend_from_slice(b"A1");
    sym_packets.extend(packet(15, &id));
    let mut past = packet(2, &special_data(-20, 720, 0x21));
    let mut vector = Vec::new();
    vector.extend_from_slice(&(-20_i16).to_be_bytes());
    vector.extend_from_slice(&720_i16.to_be_bytes());
    vector.extend_from_slice(&0_i16.to_be_bytes());
    vector.extend_from_slice(&741_i16.to_be_bytes());
    past.extend(packet(6, &vector));
    sym_packets.extend(packet(23, &past));
    sym_packets.extend(packet(24, &packet(2, &special_data(40, 740, 0x23))));

    let mut sym = Vec::new();
    sym.extend_from_slice(&(-1_i16).to_be_bytes());
    sym.extend_from_slice(&1_u16.to_be_bytes());
    sym.extend_from_slice(&0_u32.to_be_bytes());
    sym.extend_from_slice(&1_u16.to_be_bytes());
    sym.extend_from_slice(&(-1_i16).to_be_bytes());
    sym.extend_from_slice(&u32::try_from(sym_packets.len()).unwrap().to_be_bytes());
    sym.extend(sym_packets);
    let sym_len = u32::try_from(sym.len()).unwrap();
    put_u32(&mut sym, 4, sym_len);

    let mut row = vec![b' '; 80];
    put_text(&mut row, 2, "A1");
    put_text(&mut row, 9, "0/100");
    put_text(&mut row, 19, "270/ 20");
    put_text(&mut row, 29, "3/100");
    put_text(&mut row, 41, "NO DATA");
    put_text(&mut row, 51, "NO DATA");
    put_text(&mut row, 61, "NO DATA");
    put_text(&mut row, 72, "1.5/ 1.3");
    let row = String::from_utf8(row).unwrap();
    let pages = encode_pages(&[
        vec!["STORM POSITION/FORECAST".to_owned(), row],
        vec![
            "     20   (MIN) TIME (MAXIMUM)            15   (MIN) FORECAST INTERVAL".to_owned(),
            "     10         NUMBER OF PAST VOLUMES     4         NUMBER OF INTERVALS".to_owned(),
        ],
    ]);
    let mut second = base_header(101);
    let second_len = u32::try_from(120 + pages.len()).unwrap();
    put_u32(&mut second, 8, second_len);
    let mut table = Vec::new();
    table.extend_from_slice(&(-1_i16).to_be_bytes());
    table.extend_from_slice(&3_u16.to_be_bytes());
    table.extend_from_slice(&0_u32.to_be_bytes());
    table.extend(second);
    table.extend(pages);
    let table_len = u32::try_from(table.len()).unwrap();
    put_u32(&mut table, 4, table_len);

    let mut message = base_header(58);
    put_u32(&mut message, 108, 60);
    let table_offset = u32::try_from((120 + sym.len()) / 2).unwrap();
    put_u32(&mut message, 116, table_offset);
    message.extend(sym);
    message.extend(table);
    let message_len = u32::try_from(message.len()).unwrap();
    put_u32(&mut message, 8, message_len);
    with_wmo(WMO_STI, message)
}

fn structure_fixture() -> Vec<u8> {
    let mut row = vec![b' '; 80];
    put_text(&mut row, 5, "A1");
    put_text(&mut row, 13, "247/ 85");
    put_text(&mut row, 24, "< 9.5");
    put_text(&mut row, 33, "40.2");
    put_text(&mut row, 47, "65");
    put_text(&mut row, 62, "61");
    put_text(&mut row, 71, "26.8");
    structure_fixture_with_rows(&[String::from_utf8(row).unwrap()], 1)
}

fn structure_fixture_with_rows(rows: &[String], reported: u16) -> Vec<u8> {
    let mut lines = vec![
        "STORM STRUCTURE".to_owned(),
        format!(
            "     RADAR ID   1   DATE/TIME 08:22:26/23:20:10   NUMBER OF STORM CELLS {reported:>3}"
        ),
        "   STORM      AZRAN      BASE     TOP    CELL BASED VIL    MAX REF    HEIGHT".to_owned(),
    ];
    lines.extend_from_slice(rows);
    let pages = encode_pages(&[lines]);
    let mut message = base_header(62);
    put_u32(&mut message, 108, 60);
    message.extend(pages);
    let message_len = u32::try_from(message.len()).unwrap();
    put_u32(&mut message, 8, message_len);
    with_wmo(WMO_SS, message)
}

fn base_header(code: i16) -> Vec<u8> {
    let mut bytes = vec![0_u8; 120];
    put_i16(&mut bytes, 0, code);
    put_u16(&mut bytes, 2, 20_688);
    put_u32(&mut bytes, 4, 84_010);
    put_i16(&mut bytes, 12, 1);
    put_i16(&mut bytes, 14, 0);
    put_i16(&mut bytes, 16, if code == 62 { 3 } else { 5 });
    put_i16(&mut bytes, 18, -1);
    put_i32(&mut bytes, 20, 35_333);
    put_i32(&mut bytes, 24, -97_278);
    put_i16(&mut bytes, 28, 1_277);
    put_i16(&mut bytes, 30, code);
    put_i16(&mut bytes, 32, 2);
    put_i16(&mut bytes, 34, 212);
    put_i16(&mut bytes, 36, 7);
    put_i16(&mut bytes, 38, 66);
    put_u16(&mut bytes, 40, 20_688);
    put_u32(&mut bytes, 42, 84_010);
    put_u16(&mut bytes, 46, 20_688);
    put_u32(&mut bytes, 48, 84_070);
    put_u16(&mut bytes, 100, 0);
    bytes[106] = 1;
    bytes
}

fn encode_pages(pages: &[Vec<String>]) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&(-1_i16).to_be_bytes());
    bytes.extend_from_slice(&u16::try_from(pages.len()).unwrap().to_be_bytes());
    for page in pages {
        for line in page {
            assert!(line.len() <= 80);
            bytes.extend_from_slice(&u16::try_from(line.len()).unwrap().to_be_bytes());
            bytes.extend_from_slice(line.as_bytes());
        }
        bytes.extend_from_slice(&(-1_i16).to_be_bytes());
    }
    bytes
}

fn packet(code: u16, data: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&code.to_be_bytes());
    bytes.extend_from_slice(&u16::try_from(data.len()).unwrap().to_be_bytes());
    bytes.extend_from_slice(data);
    bytes
}

fn special_data(i: i16, j: i16, symbol: u8) -> [u8; 6] {
    let mut bytes = [0_u8; 6];
    bytes[..2].copy_from_slice(&i.to_be_bytes());
    bytes[2..4].copy_from_slice(&j.to_be_bytes());
    bytes[4] = symbol;
    bytes[5] = b' ';
    bytes
}

fn with_wmo(prefix: &[u8], message: Vec<u8>) -> Vec<u8> {
    let mut bytes = prefix.to_vec();
    bytes.extend(message);
    bytes
}

fn put_text(bytes: &mut [u8], offset: usize, value: &str) {
    bytes[offset..offset + value.len()].copy_from_slice(value.as_bytes());
}

fn put_i16(bytes: &mut [u8], offset: usize, value: i16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_be_bytes());
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_be_bytes());
}

fn put_i32(bytes: &mut [u8], offset: usize, value: i32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap())
}
