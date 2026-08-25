use std::collections::BTreeMap;
use std::io::Read;

use bzip2::read::BzDecoder;

use crate::{
    AzimuthRange, Compression, CoordinateProvenance, DecodeError, DecodeLimits, DecodeOptions,
    FORMAT_SPECIFICATION, ForecastPoint, GeographicPoint, HeightQualifier, NexradStormProduct,
    PRODUCT_SPECIFICATION, ProductIdentity, ProductProvenance, QualifiedHeight,
    RadarRelativePosition, SiteIdentity, SiteIdentitySource, SpecificationReferenceOwned,
    StormMotion, StormStructureCell, StormStructureProduct, StormTrackingProduct, SuppliedGeometry,
    TrackPoint, TrackedStormCell, TransportIdentity, ValidationNotice,
};

const MESSAGE_HEADER_BYTES: usize = 18;
const PRODUCT_DESCRIPTION_BYTES: usize = 102;
const MESSAGE_PREFIX_BYTES: usize = MESSAGE_HEADER_BYTES + PRODUCT_DESCRIPTION_BYTES;
const PRODUCT_STORM_TRACKING: i16 = 58;
const PRODUCT_STORM_STRUCTURE: i16 = 62;

/// Decodes a Level III product with conservative resource limits.
pub fn decode(input: &[u8]) -> Result<NexradStormProduct, DecodeError> {
    decode_with_options(input, &DecodeOptions::default())
}

/// Decodes product 58 (STI/NST) or product 62 (SS/NSS).
///
/// A short WMO/AWIPS transport prefix is detected but never trusted for
/// binary boundaries. All ROC halfword offsets and nested packet lengths are
/// checked before use, and configured limits are applied before allocation.
pub fn decode_with_options(
    input: &[u8],
    options: &DecodeOptions,
) -> Result<NexradStormProduct, DecodeError> {
    if input.is_empty() {
        return Err(DecodeError::Empty);
    }
    if input.len() > options.limits.max_input_bytes {
        return Err(DecodeError::InputLimit {
            actual: input.len(),
            limit: options.limits.max_input_bytes,
        });
    }
    let binary_start = find_message(input, options.limits.max_scan_prefix_bytes)?;
    let transport = parse_transport(&input[..binary_start]);
    let advertised_length = usize::try_from(be_u32(input, binary_start + 8, "message length")?)
        .map_err(|_| invalid("message length", binary_start + 8, "does not fit usize"))?;
    if advertised_length < MESSAGE_PREFIX_BYTES {
        return Err(invalid(
            "message length",
            binary_start + 8,
            format!("{advertised_length} is shorter than {MESSAGE_PREFIX_BYTES}"),
        ));
    }
    let message_end = binary_start
        .checked_add(advertised_length)
        .ok_or_else(|| invalid("message length", binary_start + 8, "offset overflow"))?;
    let raw_message = checked_slice(input, binary_start, advertised_length, "Level III message")?;
    let trailing = &input[message_end..];
    if !trailing
        .iter()
        .all(|byte| matches!(byte, 0 | 1 | 3 | b' ' | b'\r' | b'\n'))
    {
        return Err(invalid(
            "bytes after Level III message",
            message_end,
            format!(
                "{} non-transport bytes follow the advertised message",
                trailing.len()
            ),
        ));
    }
    let message = expand_message(raw_message, &options.limits)?;
    let parsed = parse_identity(&message, transport, options)?;

    match parsed.identity.message_code {
        PRODUCT_STORM_TRACKING => {
            parse_tracking(&message, parsed, &options.limits).map(NexradStormProduct::StormTracking)
        }
        PRODUCT_STORM_STRUCTURE => parse_structure(&message, parsed, &options.limits)
            .map(NexradStormProduct::StormStructure),
        code => Err(DecodeError::UnsupportedProduct(code)),
    }
}

fn find_message(input: &[u8], maximum_prefix: usize) -> Result<usize, DecodeError> {
    if input.len() < MESSAGE_PREFIX_BYTES {
        return Err(DecodeError::Truncated {
            context: "Level III message header and PDB",
            offset: 0,
            needed: MESSAGE_PREFIX_BYTES,
            available: input.len(),
        });
    }
    let last_start = input.len().saturating_sub(MESSAGE_PREFIX_BYTES);
    let searched = maximum_prefix.min(last_start);
    for start in 0..=searched {
        let code = i16::from_be_bytes([input[start], input[start + 1]]);
        if !matches!(code, PRODUCT_STORM_TRACKING | PRODUCT_STORM_STRUCTURE) {
            continue;
        }
        let divider = i16::from_be_bytes([input[start + 18], input[start + 19]]);
        let pdb_code = i16::from_be_bytes([input[start + 30], input[start + 31]]);
        if divider == -1 && pdb_code == code {
            return Ok(start);
        }
    }
    Err(DecodeError::MessageNotFound {
        searched: maximum_prefix.min(input.len()),
    })
}

fn expand_message(raw: &[u8], limits: &DecodeLimits) -> Result<Vec<u8>, DecodeError> {
    // ROC 2620001AD Figure 3-6 sheet 1 places P8 at absolute byte 100,
    // P9/P10 at bytes 102..106, and starts compressed blocks after byte 120.
    // Appendix D assigns P8=1 to bzip2.
    let method = be_u16(raw, 100, "PDB compression method")?;
    match method {
        0 => Ok(raw.to_vec()),
        1 => {
            let expected = usize::try_from(be_u32(raw, 102, "uncompressed body size")?)
                .map_err(|_| invalid("uncompressed body size", 102, "does not fit usize"))?;
            if expected > limits.max_decompressed_body_bytes {
                return Err(DecodeError::Limit {
                    collection: "decompressed body bytes",
                    limit: limits.max_decompressed_body_bytes,
                });
            }
            let compressed = checked_slice(
                raw,
                MESSAGE_PREFIX_BYTES,
                raw.len() - MESSAGE_PREFIX_BYTES,
                "compressed message body",
            )?;
            let read_limit = expected
                .min(limits.max_decompressed_body_bytes)
                .checked_add(1)
                .ok_or_else(|| invalid("decompression limit", 102, "overflow"))?;
            let mut decoded = Vec::with_capacity(expected.min(1024 * 1024));
            BzDecoder::new(compressed)
                .take(u64::try_from(read_limit).unwrap_or(u64::MAX))
                .read_to_end(&mut decoded)
                .map_err(|error| DecodeError::Decompression(error.to_string()))?;
            if decoded.len() != expected {
                return Err(DecodeError::DecompressedSize {
                    expected,
                    actual: decoded.len(),
                });
            }
            let total = MESSAGE_PREFIX_BYTES
                .checked_add(decoded.len())
                .ok_or_else(|| invalid("expanded message", 0, "length overflow"))?;
            let mut expanded = Vec::with_capacity(total);
            expanded.extend_from_slice(&raw[..MESSAGE_PREFIX_BYTES]);
            expanded.extend_from_slice(&decoded);
            Ok(expanded)
        }
        method => Err(DecodeError::UnsupportedCompression {
            method,
            offset: 100,
        }),
    }
}

struct ParsedIdentity {
    identity: ProductIdentity,
    symbology_offset: Option<usize>,
    graphic_offset: Option<usize>,
    tabular_offset: Option<usize>,
}

fn parse_identity(
    message: &[u8],
    transport: TransportIdentity,
    options: &DecodeOptions,
) -> Result<ParsedIdentity, DecodeError> {
    checked_slice(message, 0, MESSAGE_PREFIX_BYTES, "message header and PDB")?;
    let code = be_i16(message, 0, "message code")?;
    if !matches!(code, PRODUCT_STORM_TRACKING | PRODUCT_STORM_STRUCTURE) {
        return Err(DecodeError::UnsupportedProduct(code));
    }
    expect_i16(message, 18, -1, "PDB divider")?;
    if be_i16(message, 30, "PDB product code")? != code {
        return Err(invalid(
            "PDB product code",
            30,
            "does not match message header",
        ));
    }
    let radar = GeographicPoint {
        latitude_degrees: f64::from(be_i32(message, 20, "radar latitude")?) / 1_000.0,
        longitude_degrees: f64::from(be_i32(message, 24, "radar longitude")?) / 1_000.0,
    };
    if !(-90.0..=90.0).contains(&radar.latitude_degrees)
        || !(-180.0..=180.0).contains(&radar.longitude_degrees)
    {
        return Err(invalid(
            "radar coordinates",
            20,
            "outside geographic bounds",
        ));
    }
    let compression = match be_u16(message, 100, "PDB compression method")? {
        0 => Compression::None,
        1 => Compression::Bzip2,
        method => {
            return Err(DecodeError::UnsupportedCompression {
                method,
                offset: 100,
            });
        }
    };
    let site = site_identity(options.site_hint.as_deref(), &transport)?;
    let volume_scan_at_unix_ms = roc_time(
        be_u16(message, 40, "volume date")?,
        be_u32(message, 42, "volume time")?,
        40,
    )?;
    let generated_at_unix_ms = roc_time(
        be_u16(message, 46, "generation date")?,
        be_u32(message, 48, "generation time")?,
        46,
    )?;
    let supplied_geometry = if code == PRODUCT_STORM_TRACKING {
        SuppliedGeometry::CentroidPointsAndTracks
    } else {
        SuppliedGeometry::CentroidPointsOnly
    };
    let identity = ProductIdentity {
        message_code: code,
        mnemonic: if code == PRODUCT_STORM_TRACKING {
            "NST/STI".to_owned()
        } else {
            "NSS/SS".to_owned()
        },
        product_version: message[106],
        radar_site: site,
        radar_location: radar,
        radar_height_feet: be_i16(message, 28, "radar height")?,
        message_at_unix_ms: roc_time(
            be_u16(message, 2, "message date")?,
            be_u32(message, 4, "message time")?,
            2,
        )?,
        volume_scan_at_unix_ms,
        generated_at_unix_ms,
        message_sequence: be_i16(message, 36, "message sequence")?,
        volume_scan_number: be_i16(message, 38, "volume scan number")?,
        source_id: be_i16(message, 12, "source ID")?,
        destination_id: be_i16(message, 14, "destination ID")?,
        operational_mode: be_i16(message, 32, "operational mode")?,
        volume_coverage_pattern: be_i16(message, 34, "VCP")?,
        compression,
        transport,
        provenance: ProductProvenance {
            producer: "WSR-88D Radar Product Generator".to_owned(),
            format_specification: SpecificationReferenceOwned::from(FORMAT_SPECIFICATION),
            product_specification: SpecificationReferenceOwned::from(PRODUCT_SPECIFICATION),
            supplied_geometry,
            geometry_statement: "This Level III product supplies centroid points and, for product 58, point tracks. It does not supply storm polygons.".to_owned(),
        },
        validation_notices: Vec::new(),
    };

    Ok(ParsedIdentity {
        identity,
        symbology_offset: halfword_offset(message, 108, "first/symbology block offset")?,
        graphic_offset: raw_halfword_offset(message, 112, "graphic/cell-trend offset")?,
        tabular_offset: halfword_offset(message, 116, "tabular block offset")?,
    })
}

fn parse_transport(prefix: &[u8]) -> TransportIdentity {
    if prefix.is_empty() || !prefix.iter().all(u8::is_ascii) {
        return TransportIdentity::default();
    }
    let normalized: String = prefix
        .iter()
        .copied()
        .filter(|byte| !matches!(byte, 0 | 1 | 3))
        .map(char::from)
        .collect();
    let lines: Vec<&str> = normalized
        .split(['\r', '\n'])
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let first = lines.first().copied();
    let first_tokens: Vec<&str> = first.unwrap_or_default().split_ascii_whitespace().collect();
    let product_identifier = lines
        .iter()
        .flat_map(|line| line.split_ascii_whitespace())
        .find(|token| {
            token.len() == 6
                && (token.starts_with("NST") || token.starts_with("NSS"))
                && token.bytes().all(|byte| byte.is_ascii_alphanumeric())
        })
        .map(str::to_owned);
    TransportIdentity {
        wmo_heading: first_tokens.first().map(|value| (*value).to_owned()),
        wmo_origin: first_tokens.get(1).map(|value| (*value).to_owned()),
        product_identifier,
    }
}

fn site_identity(
    hint: Option<&str>,
    transport: &TransportIdentity,
) -> Result<SiteIdentity, DecodeError> {
    if let Some(hint) = hint {
        if !(3..=4).contains(&hint.len()) || !hint.bytes().all(|byte| byte.is_ascii_alphanumeric())
        {
            return Err(invalid(
                "site hint",
                0,
                "must be a 3- or 4-character ASCII identifier",
            ));
        }
        return Ok(SiteIdentity {
            site_id: Some(hint.to_ascii_uppercase()),
            source: SiteIdentitySource::CallerHint,
        });
    }
    if let Some(pil) = &transport.product_identifier {
        return Ok(SiteIdentity {
            // The AWIPS PIL transmits the three-character radar token. Do not
            // invent a K/P/T ICAO prefix that is absent from the product.
            site_id: pil.get(3..).map(str::to_owned),
            source: SiteIdentitySource::WmoProductIdentifier,
        });
    }
    Ok(SiteIdentity {
        site_id: None,
        source: SiteIdentitySource::SourceIdOnly,
    })
}

fn roc_time(date: u16, seconds: u32, offset: usize) -> Result<i64, DecodeError> {
    if date == 0 {
        return Err(invalid("ROC date", offset, "day count is zero"));
    }
    if seconds >= 86_400 {
        return Err(invalid(
            "ROC time",
            offset + 2,
            format!("{seconds} seconds is outside one UTC day"),
        ));
    }
    let days = i64::from(date - 1);
    days.checked_mul(86_400_000)
        .and_then(|value| value.checked_add(i64::from(seconds) * 1_000))
        .ok_or_else(|| invalid("ROC timestamp", offset, "overflow"))
}

fn parse_tracking(
    message: &[u8],
    parsed: ParsedIdentity,
    limits: &DecodeLimits,
) -> Result<StormTrackingProduct, DecodeError> {
    let symbology_offset = parsed.symbology_offset.ok_or_else(|| {
        invalid(
            "symbology block offset",
            108,
            "product 58 requires a symbology block",
        )
    })?;
    let tabular_offset = parsed.tabular_offset.ok_or_else(|| {
        invalid(
            "tabular block offset",
            116,
            "product 58 requires its paired alphanumeric block",
        )
    })?;
    if symbology_offset < MESSAGE_PREFIX_BYTES || tabular_offset < MESSAGE_PREFIX_BYTES {
        return Err(invalid(
            "product block offset",
            108,
            "points into the message header/PDB",
        ));
    }
    let symbology_end = block_envelope_end(message, symbology_offset, 1, "symbology block")?;
    let next_after_symbology = parsed.graphic_offset.unwrap_or(tabular_offset);
    if symbology_end != next_after_symbology {
        return Err(invalid(
            "symbology block ordering",
            symbology_end,
            format!("next declared block begins at {next_after_symbology}"),
        ));
    }
    if let Some(graphic_offset) = parsed.graphic_offset {
        if graphic_offset < MESSAGE_PREFIX_BYTES || graphic_offset >= message.len() {
            return Err(invalid(
                "graphic block offset",
                112,
                format!("byte offset {graphic_offset} is outside the message body"),
            ));
        }
        let graphic_end = block_envelope_end(message, graphic_offset, 2, "graphic block")?;
        if graphic_end != tabular_offset {
            return Err(invalid(
                "graphic block ordering",
                graphic_end,
                format!("tabular block begins at {tabular_offset}"),
            ));
        }
    }
    let drafts = parse_tracking_symbology(message, symbology_offset, limits)?;
    let table = parse_paired_table(message, tabular_offset, limits)?;
    if drafts.len() > limits.max_cells || table.rows.len() > limits.max_cells {
        return Err(DecodeError::Limit {
            collection: "storm cells",
            limit: limits.max_cells,
        });
    }

    let mut rows: BTreeMap<String, TrackingRow> = table
        .rows
        .into_iter()
        .map(|row| (row.storm_id.clone(), row))
        .collect();
    let mut cells = Vec::with_capacity(drafts.len());
    for draft in drafts {
        let storm_id = draft.storm_id.ok_or_else(|| {
            invalid(
                "storm ID packet",
                symbology_offset,
                "a current centroid has no packet 15 identifier",
            )
        })?;
        let row = rows.remove(&storm_id).ok_or_else(|| {
            DecodeError::CrossCheck(format!(
                "symbology storm '{storm_id}' is absent from the paired table"
            ))
        })?;
        if let Some(tabular) = row.current {
            cross_check_azimuth_range(
                draft.current,
                tabular,
                &storm_id,
                parsed.identity.radar_location,
            )?;
        }
        let tabular_forecast_count = row
            .forecasts
            .iter()
            .take_while(|position| position.is_some())
            .count();
        if row
            .forecasts
            .iter()
            .skip(tabular_forecast_count)
            .any(Option::is_some)
        {
            return Err(DecodeError::CrossCheck(format!(
                "storm {storm_id} has a tabular forecast after a NO DATA interval"
            )));
        }
        if tabular_forecast_count != draft.forecasts.len() {
            return Err(DecodeError::CrossCheck(format!(
                "storm {storm_id} has {} packet forecasts but {tabular_forecast_count} tabular forecasts",
                draft.forecasts.len()
            )));
        }
        for (exact, rounded) in draft.forecasts.iter().zip(row.forecasts.iter().flatten()) {
            cross_check_azimuth_range(*exact, *rounded, &storm_id, parsed.identity.radar_location)?;
        }
        let current = make_track_point(
            draft.current,
            parsed.identity.radar_location,
            Some(parsed.identity.volume_scan_at_unix_ms),
        );
        let history_in_packet_order = draft
            .history
            .into_iter()
            .map(|position| make_track_point(position, parsed.identity.radar_location, None))
            .collect();
        let forecasts = draft
            .forecasts
            .into_iter()
            .enumerate()
            .map(|(index, position)| {
                let lead_minutes = table.forecast_interval_minutes.and_then(|interval| {
                    u16::try_from(index + 1)
                        .ok()
                        .and_then(|number| interval.checked_mul(number))
                });
                ForecastPoint {
                    point: make_track_point(
                        position,
                        parsed.identity.radar_location,
                        lead_minutes.map(|minutes| {
                            parsed
                                .identity
                                .volume_scan_at_unix_ms
                                .saturating_add(i64::from(minutes) * 60_000)
                        }),
                    ),
                    lead_minutes,
                    tabular_position: row.forecasts.get(index).copied().flatten(),
                }
            })
            .collect();
        cells.push(TrackedStormCell {
            storm_id,
            current,
            history_in_packet_order,
            forecasts,
            motion: row.motion,
            forecast_error_nautical_miles: row.forecast_error,
            mean_error_nautical_miles: row.mean_error,
            stationary_radius_quarter_km: draft.stationary_radius,
            tabular_current: row.current,
        });
    }
    if !rows.is_empty() {
        return Err(DecodeError::CrossCheck(format!(
            "paired table contains storm IDs with no current symbology centroid: {}",
            rows.keys().cloned().collect::<Vec<_>>().join(", ")
        )));
    }
    cells.sort_by(|left, right| left.storm_id.cmp(&right.storm_id));
    Ok(StormTrackingProduct {
        identity: parsed.identity,
        cells,
        forecast_interval_minutes: table.forecast_interval_minutes,
        number_of_past_volumes: table.number_of_past_volumes,
    })
}

#[derive(Default)]
struct TrackingDraft {
    current: RadarRelativePosition,
    storm_id: Option<String>,
    history: Vec<RadarRelativePosition>,
    forecasts: Vec<RadarRelativePosition>,
    stationary_radius: Option<i16>,
}

fn parse_tracking_symbology(
    message: &[u8],
    offset: usize,
    limits: &DecodeLimits,
) -> Result<Vec<TrackingDraft>, DecodeError> {
    // ROC 2620001AD Figure 3-6 sheet 3: divider, block ID=1,
    // inclusive block length, layer count; each layer has its own length.
    expect_i16(message, offset, -1, "symbology divider")?;
    expect_u16(message, offset + 2, 1, "symbology block ID")?;
    let block_length = usize::try_from(be_u32(message, offset + 4, "symbology block length")?)
        .map_err(|_| invalid("symbology block length", offset + 4, "does not fit usize"))?;
    if block_length < 10 {
        return Err(invalid(
            "symbology block length",
            offset + 4,
            "shorter than its 10-byte header",
        ));
    }
    let block_end = checked_end(message, offset, block_length, "symbology block")?;
    let layers = usize::from(be_u16(message, offset + 8, "symbology layer count")?);
    if layers > limits.max_layers {
        return Err(DecodeError::Limit {
            collection: "symbology layers",
            limit: limits.max_layers,
        });
    }
    let mut cursor = offset + 10;
    let mut drafts = Vec::new();
    let mut current: Option<TrackingDraft> = None;
    let mut storm_ids = BTreeMap::<RadarRelativePosition, String>::new();
    for _ in 0..layers {
        expect_i16_bounded(message, cursor, -1, "layer divider", block_end)?;
        let layer_length = usize::try_from(be_u32_bounded(
            message,
            cursor + 2,
            "layer length",
            block_end,
        )?)
        .map_err(|_| invalid("layer length", cursor + 2, "does not fit usize"))?;
        cursor = cursor
            .checked_add(6)
            .ok_or_else(|| invalid("layer offset", cursor, "overflow"))?;
        let layer_end =
            checked_end_bounded(message, cursor, layer_length, block_end, "symbology layer")?;
        let mut packets = 0usize;
        while cursor < layer_end {
            packets += 1;
            if packets > limits.max_packets_per_layer {
                return Err(DecodeError::Limit {
                    collection: "packets per layer",
                    limit: limits.max_packets_per_layer,
                });
            }
            let code = be_u16_bounded(message, cursor, "packet code", layer_end)?;
            let data_length = usize::from(be_u16_bounded(
                message,
                cursor + 2,
                "packet data length",
                layer_end,
            )?);
            let data_start = cursor + 4;
            let data_end =
                checked_end_bounded(message, data_start, data_length, layer_end, "packet data")?;
            let data = &message[data_start..data_end];
            match code {
                2 => {
                    let (position, symbol) = parse_special_symbol(data, data_start)?;
                    if symbol == 0x22 {
                        if let Some(finished) = current.take() {
                            push_draft(&mut drafts, finished, limits)?;
                        }
                        current = Some(TrackingDraft {
                            current: position,
                            ..TrackingDraft::default()
                        });
                    }
                }
                15 => {
                    let entries = parse_storm_ids(data, data_start)?;
                    for (position, storm_id) in entries {
                        if storm_ids.values().any(|existing| existing == &storm_id) {
                            return Err(invalid(
                                "storm ID packet",
                                cursor,
                                format!("duplicates storm ID {storm_id}"),
                            ));
                        }
                        if storm_ids.insert(position, storm_id).is_some() {
                            return Err(invalid(
                                "storm ID packet",
                                cursor,
                                "duplicates a storm centroid position",
                            ));
                        }
                    }
                }
                23 | 24 => {
                    let expected_symbol = if code == 23 { 0x21 } else { 0x23 };
                    let positions = parse_nested_track_packets(
                        data,
                        data_start,
                        expected_symbol,
                        limits.max_track_points_per_cell,
                    )?;
                    let draft = current.as_mut().ok_or_else(|| {
                        invalid(
                            "track packet",
                            cursor,
                            "appears before a current-position packet",
                        )
                    })?;
                    let target = if code == 23 {
                        &mut draft.history
                    } else {
                        &mut draft.forecasts
                    };
                    if target.len().saturating_add(positions.len())
                        > limits.max_track_points_per_cell
                    {
                        return Err(DecodeError::Limit {
                            collection: "track points per cell",
                            limit: limits.max_track_points_per_cell,
                        });
                    }
                    target.extend(positions);
                }
                25 => {
                    // Figure 3-14 sheet 4: I, J, and radius after the header.
                    if data.len() != 6 {
                        return Err(invalid(
                            "packet 25 length",
                            cursor + 2,
                            format!("expected 6, got {}", data.len()),
                        ));
                    }
                    let position = RadarRelativePosition {
                        i_quarter_km: slice_i16(data, 0),
                        j_quarter_km: slice_i16(data, 2),
                    };
                    if let Some(finished) = current.take() {
                        push_draft(&mut drafts, finished, limits)?;
                    }
                    current = Some(TrackingDraft {
                        current: position,
                        stationary_radius: Some(slice_i16(data, 4)),
                        ..TrackingDraft::default()
                    });
                }
                6 => {
                    validate_vector_packet(data, data_start)?;
                }
                other => {
                    return Err(invalid(
                        "STI symbology packet code",
                        cursor,
                        format!("unsupported packet {other}"),
                    ));
                }
            }
            cursor = data_end;
        }
        if cursor != layer_end {
            return Err(invalid(
                "symbology layer",
                cursor,
                "packet traversal did not end on the layer boundary",
            ));
        }
    }
    if let Some(finished) = current {
        push_draft(&mut drafts, finished, limits)?;
    }
    if cursor != block_end {
        return Err(invalid(
            "symbology block",
            cursor,
            format!("{} trailing bytes", block_end - cursor),
        ));
    }
    for draft in &mut drafts {
        draft.storm_id = storm_ids.remove(&draft.current);
    }
    if !storm_ids.is_empty() {
        return Err(invalid(
            "storm ID packet",
            offset,
            "contains an ID with no matching current centroid",
        ));
    }
    Ok(drafts)
}

fn push_draft(
    drafts: &mut Vec<TrackingDraft>,
    draft: TrackingDraft,
    limits: &DecodeLimits,
) -> Result<(), DecodeError> {
    if drafts.len() >= limits.max_cells {
        return Err(DecodeError::Limit {
            collection: "storm cells",
            limit: limits.max_cells,
        });
    }
    drafts.push(draft);
    Ok(())
}

fn parse_special_symbol(
    data: &[u8],
    absolute_offset: usize,
) -> Result<(RadarRelativePosition, u8), DecodeError> {
    // ROC 2620001AD Figure 3-8b sheet 2: packet 2 carries I/J and two
    // character bytes. 0x21/0x22/0x23 are past/current/forecast marks.
    if data.len() != 6 {
        return Err(invalid(
            "packet 2 length",
            absolute_offset.saturating_sub(2),
            format!("expected 6, got {}", data.len()),
        ));
    }
    Ok((
        RadarRelativePosition {
            i_quarter_km: slice_i16(data, 0),
            j_quarter_km: slice_i16(data, 2),
        },
        data[4],
    ))
}

fn parse_storm_ids(
    data: &[u8],
    absolute_offset: usize,
) -> Result<Vec<(RadarRelativePosition, String)>, DecodeError> {
    // ROC 2620001AD Figure 3-14 sheet 1: repeated I, J, two-character ID.
    if data.is_empty() || !data.len().is_multiple_of(6) {
        return Err(invalid(
            "packet 15 length",
            absolute_offset.saturating_sub(2),
            format!("{} is not a positive multiple of 6", data.len()),
        ));
    }
    data.chunks_exact(6)
        .enumerate()
        .map(|(index, entry)| {
            let storm_id = String::from_utf8_lossy(&entry[4..6]).into_owned();
            if !valid_storm_id(&storm_id) {
                return Err(DecodeError::NonAscii {
                    context: "packet 15 storm ID",
                    offset: absolute_offset + index * 6 + 4,
                });
            }
            Ok((
                RadarRelativePosition {
                    i_quarter_km: slice_i16(entry, 0),
                    j_quarter_km: slice_i16(entry, 2),
                },
                storm_id,
            ))
        })
        .collect()
}

fn parse_nested_track_packets(
    data: &[u8],
    absolute_offset: usize,
    expected_symbol: u8,
    maximum_points: usize,
) -> Result<Vec<RadarRelativePosition>, DecodeError> {
    let mut cursor = 0usize;
    let mut positions = Vec::new();
    while cursor < data.len() {
        let code = local_u16(data, cursor, absolute_offset, "nested packet code")?;
        let length = usize::from(local_u16(
            data,
            cursor + 2,
            absolute_offset,
            "nested packet length",
        )?);
        let data_start = cursor + 4;
        let data_end = data_start.checked_add(length).ok_or_else(|| {
            invalid(
                "nested packet length",
                absolute_offset + cursor + 2,
                "overflow",
            )
        })?;
        if data_end > data.len() {
            return Err(DecodeError::Truncated {
                context: "nested track packet",
                offset: absolute_offset + data_start,
                needed: length,
                available: data.len().saturating_sub(data_start),
            });
        }
        match code {
            2 => {
                let (position, symbol) = parse_special_symbol(
                    &data[data_start..data_end],
                    absolute_offset + data_start,
                )?;
                if symbol != expected_symbol {
                    return Err(invalid(
                        "nested track symbol",
                        absolute_offset + data_start + 4,
                        format!("expected 0x{expected_symbol:02x}, got 0x{symbol:02x}"),
                    ));
                }
                if positions.len() >= maximum_points {
                    return Err(DecodeError::Limit {
                        collection: "track points per cell",
                        limit: maximum_points,
                    });
                }
                positions.push(position);
            }
            6 => validate_vector_packet(&data[data_start..data_end], absolute_offset + data_start)?,
            other => {
                return Err(invalid(
                    "nested track packet code",
                    absolute_offset + cursor,
                    format!("unsupported packet {other}"),
                ));
            }
        }
        cursor = data_end;
    }
    Ok(positions)
}

fn validate_vector_packet(data: &[u8], absolute_offset: usize) -> Result<(), DecodeError> {
    // Packet 6 is a sequence of I/J endpoints (Figure 3-7). We do not infer
    // extra centroids from its drawing vectors, but validate its framing.
    if data.is_empty() || !data.len().is_multiple_of(4) {
        return Err(invalid(
            "packet 6 vector length",
            absolute_offset.saturating_sub(2),
            format!("{} is not a positive multiple of 4", data.len()),
        ));
    }
    Ok(())
}

fn make_track_point(
    position: RadarRelativePosition,
    radar: GeographicPoint,
    valid_at_unix_ms: Option<i64>,
) -> TrackPoint {
    TrackPoint {
        position,
        geographic: position.geographic_from(radar),
        valid_at_unix_ms,
        radar_relative_provenance: CoordinateProvenance::RpgPacketQuarterKilometre,
        geographic_derivation: CoordinateProvenance::SphericalRadarCentricRwV1,
    }
}

struct TrackingTable {
    rows: Vec<TrackingRow>,
    forecast_interval_minutes: Option<u16>,
    number_of_past_volumes: Option<u16>,
}

struct TrackingRow {
    storm_id: String,
    current: Option<AzimuthRange>,
    motion: StormMotion,
    forecasts: Vec<Option<AzimuthRange>>,
    forecast_error: Option<f32>,
    mean_error: Option<f32>,
}

fn parse_paired_table(
    message: &[u8],
    offset: usize,
    limits: &DecodeLimits,
) -> Result<TrackingTable, DecodeError> {
    // ROC 2620001AD Figure 3-6 sheets 6-7: the ID=3 block contains a second
    // message header/PDB, then divider/pages/length-prefixed ASCII lines.
    expect_i16(message, offset, -1, "tabular divider")?;
    expect_u16(message, offset + 2, 3, "tabular block ID")?;
    let block_length = usize::try_from(be_u32(message, offset + 4, "tabular block length")?)
        .map_err(|_| invalid("tabular block length", offset + 4, "does not fit usize"))?;
    if block_length < 8 + MESSAGE_PREFIX_BYTES + 4 {
        return Err(invalid(
            "tabular block length",
            offset + 4,
            "shorter than paired table headers",
        ));
    }
    let block_end = checked_end(message, offset, block_length, "tabular block")?;
    if block_end != message.len() {
        return Err(invalid(
            "tabular block length",
            offset + 4,
            format!(
                "block ends at {block_end}, but the required final block must end at {}",
                message.len()
            ),
        ));
    }
    let second_header = offset + 8;
    expect_i16_bounded(
        message,
        second_header,
        101,
        "paired alphanumeric message code",
        block_end,
    )?;
    let second_length = usize::try_from(be_u32_bounded(
        message,
        second_header + 8,
        "paired alphanumeric message length",
        block_end,
    )?)
    .map_err(|_| {
        invalid(
            "paired alphanumeric message length",
            second_header + 8,
            "does not fit usize",
        )
    })?;
    if second_length != block_length - 8 {
        return Err(invalid(
            "paired alphanumeric message length",
            second_header + 8,
            format!("expected {}, got {second_length}", block_length - 8),
        ));
    }
    expect_i16_bounded(
        message,
        second_header + 18,
        -1,
        "paired PDB divider",
        block_end,
    )?;
    expect_i16_bounded(
        message,
        second_header + 30,
        101,
        "paired alphanumeric PDB product code",
        block_end,
    )?;
    let page_start = second_header + MESSAGE_PREFIX_BYTES;
    let (pages, end) = parse_pages(message, page_start, block_end, limits)?;
    if end != block_end {
        return Err(invalid(
            "tabular block",
            end,
            format!("{} trailing bytes", block_end - end),
        ));
    }
    let mut rows = Vec::new();
    let mut forecast_interval_minutes = None;
    let mut number_of_past_volumes = None;
    for page in &pages {
        for line in page {
            if forecast_interval_minutes.is_none() && line.contains("FORECAST INTERVAL") {
                forecast_interval_minutes = number_before_keyword(line, "FORECAST INTERVAL");
            }
            if number_of_past_volumes.is_none() && line.contains("NUMBER OF PAST VOLUMES") {
                number_of_past_volumes = number_before_keyword(line, "NUMBER OF PAST VOLUMES");
            }
            if let Some(row) = parse_tracking_row(line)? {
                if rows
                    .iter()
                    .any(|existing: &TrackingRow| existing.storm_id == row.storm_id)
                {
                    return Err(invalid(
                        "tracking table",
                        offset,
                        format!("duplicate storm ID {}", row.storm_id),
                    ));
                }
                rows.push(row);
            }
        }
    }
    Ok(TrackingTable {
        rows,
        forecast_interval_minutes,
        number_of_past_volumes,
    })
}

fn parse_tracking_row(line: &str) -> Result<Option<TrackingRow>, DecodeError> {
    let bytes = line.as_bytes();
    if bytes.len() < 28 {
        return Ok(None);
    }
    let storm_id = ascii_column(bytes, 0, 8).trim();
    if !valid_storm_id(storm_id) {
        return Ok(None);
    }
    let current_text = ascii_column(bytes, 8, 18).trim();
    if !current_text.contains('/') {
        return Ok(None);
    }
    let current = parse_optional_azimuth_range(current_text, "tracking current position")?;
    let motion_text = ascii_column(bytes, 18, 28).trim();
    let motion = if motion_text.eq_ignore_ascii_case("NEW") {
        StormMotion::New
    } else if motion_text.eq_ignore_ascii_case("NO DATA") || motion_text.is_empty() {
        StormMotion::NoData
    } else {
        let movement = parse_azimuth_range(motion_text, "storm motion")?;
        StormMotion::Moving {
            direction_from_degrees: movement.azimuth_degrees,
            speed_knots: movement.range_nautical_miles,
        }
    };
    let forecasts = [(28, 38), (38, 48), (48, 58), (58, 68)]
        .into_iter()
        .map(|(start, end)| {
            parse_optional_azimuth_range(
                ascii_column(bytes, start, end).trim(),
                "tracking forecast position",
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    let error_text = ascii_column(bytes, 70, 80).trim();
    let (forecast_error, mean_error) =
        if error_text.is_empty() || error_text.eq_ignore_ascii_case("NO DATA") {
            (None, None)
        } else {
            let (left, right) = split_once_required(error_text, '/', "tracking forecast error")?;
            (
                Some(parse_f32(left, "forecast error")?),
                Some(parse_f32(right, "mean error")?),
            )
        };
    if forecast_error.is_some_and(|value| value < 0.0)
        || mean_error.is_some_and(|value| value < 0.0)
    {
        return Err(invalid(
            "tracking forecast error",
            0,
            "errors must be non-negative",
        ));
    }
    Ok(Some(TrackingRow {
        storm_id: storm_id.to_owned(),
        current,
        motion,
        forecasts,
        forecast_error,
        mean_error,
    }))
}

fn parse_pages(
    message: &[u8],
    offset: usize,
    boundary: usize,
    limits: &DecodeLimits,
) -> Result<(Vec<Vec<String>>, usize), DecodeError> {
    expect_i16_bounded(message, offset, -1, "alphanumeric divider", boundary)?;
    let count = usize::from(be_u16_bounded(
        message,
        offset + 2,
        "alphanumeric page count",
        boundary,
    )?);
    if count == 0 || count > 48 || count > limits.max_pages {
        return Err(DecodeError::Limit {
            collection: "alphanumeric pages",
            limit: limits.max_pages.min(48),
        });
    }
    let mut cursor = offset + 4;
    let mut pages = Vec::with_capacity(count);
    for _ in 0..count {
        let mut page = Vec::new();
        loop {
            let length_or_end =
                be_i16_bounded(message, cursor, "line length or page divider", boundary)?;
            cursor += 2;
            if length_or_end == -1 {
                break;
            }
            if !(0..=80).contains(&length_or_end) {
                return Err(invalid(
                    "alphanumeric line length",
                    cursor - 2,
                    format!("{length_or_end} is outside 0..=80"),
                ));
            }
            if page.len() >= 17 || page.len() >= limits.max_lines_per_page {
                return Err(DecodeError::Limit {
                    collection: "lines per page",
                    limit: limits.max_lines_per_page.min(17),
                });
            }
            let length = usize::try_from(length_or_end).unwrap_or_default();
            let line_bytes =
                checked_slice_bounded(message, cursor, length, boundary, "alphanumeric line")?;
            if !line_bytes.iter().all(|byte| (byte & 0x7f).is_ascii()) {
                return Err(DecodeError::NonAscii {
                    context: "alphanumeric line",
                    offset: cursor,
                });
            }
            // ROC allows the high bit to select a display special character.
            // Mask it for the canonical textual table while packet geometry
            // remains the authoritative machine-readable position source.
            let normalized: Vec<u8> = line_bytes.iter().map(|byte| byte & 0x7f).collect();
            page.push(String::from_utf8_lossy(&normalized).into_owned());
            cursor += length;
        }
        pages.push(page);
    }
    Ok((pages, cursor))
}

fn parse_structure(
    message: &[u8],
    mut parsed: ParsedIdentity,
    limits: &DecodeLimits,
) -> Result<StormStructureProduct, DecodeError> {
    // ROC 2620001AD section 3.3.2/Figure 3-16: product 62 is a stand-alone
    // table; the first PDB offset points directly to divider/pages.
    let offset = parsed.symbology_offset.ok_or_else(|| {
        invalid(
            "stand-alone table offset",
            108,
            "product 62 requires its table",
        )
    })?;
    if offset < MESSAGE_PREFIX_BYTES {
        return Err(invalid(
            "stand-alone table offset",
            108,
            "points into the message header/PDB",
        ));
    }
    let usable_graphic_offset = match parsed.graphic_offset {
        Some(value) if value <= message.len() => Some(value),
        Some(value) => {
            // Zero-cell operational products have been observed with a stale
            // optional trend offset beyond the message, while their complete
            // Figure-3-16 table consumes the message exactly. Keep that fact
            // visible instead of either failing the useful table or silently
            // pretending the offset was valid.
            parsed.identity.validation_notices.push(
                ValidationNotice::IgnoredOutOfRangeOptionalCellTrendOffset {
                    offset_bytes: value,
                    message_length: message.len(),
                },
            );
            None
        }
        None => None,
    };
    let trend_packet_start = usable_graphic_offset.map(|value| {
        // Figure 3-6 sheet 6 notes that product 62's Graphic offset points
        // to Cell Trend data. Operational RPG products point to packet 22's
        // length halfword, so its packet-code halfword is immediately prior.
        value.saturating_sub(2)
    });
    let table_boundary = trend_packet_start.unwrap_or(message.len());
    if table_boundary < offset {
        return Err(invalid(
            "cell-trend offset",
            112,
            "precedes the stand-alone table",
        ));
    }
    let (pages, end) = parse_pages(message, offset, table_boundary, limits)?;
    if end != table_boundary {
        return Err(invalid(
            "stand-alone table",
            end,
            format!("{} trailing bytes before cell trends", table_boundary - end),
        ));
    }
    if let Some(trend_start) = trend_packet_start {
        validate_cell_trends(message, trend_start, limits.max_cells)?;
    }
    let mut cells = Vec::new();
    let mut reported_cell_count = None;
    for page in pages {
        for line in page {
            if reported_cell_count.is_none() && line.contains("NUMBER OF STORM CELLS") {
                reported_cell_count = number_after_keyword(&line, "NUMBER OF STORM CELLS");
            }
            if let Some(cell) = parse_structure_row(&line)? {
                if cells.len() >= limits.max_cells {
                    return Err(DecodeError::Limit {
                        collection: "storm structure cells",
                        limit: limits.max_cells,
                    });
                }
                if cells
                    .iter()
                    .any(|existing: &StormStructureCell| existing.storm_id == cell.storm_id)
                {
                    return Err(invalid(
                        "storm structure table",
                        offset,
                        format!("duplicate storm ID {}", cell.storm_id),
                    ));
                }
                cells.push(cell);
            }
        }
    }
    if let Some(reported) = reported_cell_count
        && usize::from(reported) != cells.len()
    {
        return Err(DecodeError::CrossCheck(format!(
            "Storm Structure reports {reported} cells but {} rows decoded",
            cells.len()
        )));
    }
    cells.sort_by(|left, right| left.storm_id.cmp(&right.storm_id));
    Ok(StormStructureProduct {
        identity: parsed.identity,
        cells,
        reported_cell_count,
    })
}

fn validate_cell_trends(
    message: &[u8],
    offset: usize,
    maximum_cells: usize,
) -> Result<(), DecodeError> {
    // ROC 2620001AD Figures 3-15 and 3-15a. Attributes exposed by this crate
    // come from the Format-V table; this pass validates the accompanying
    // trend packet framing so malformed tails cannot be silently accepted.
    let mut cursor = offset;
    expect_u16(message, cursor, 22, "cell-trend volume-time packet code")?;
    let times_length = usize::from(be_u16(
        message,
        cursor + 2,
        "cell-trend volume-time packet length",
    )?);
    if !(4..=22).contains(&times_length) || !times_length.is_multiple_of(2) {
        return Err(invalid(
            "cell-trend volume-time packet length",
            cursor + 2,
            format!("{times_length} is outside the ROC packet bounds"),
        ));
    }
    cursor = checked_end(message, cursor + 4, times_length, "cell-trend volume times")?;
    let mut cells = 0usize;
    while cursor < message.len() {
        if cells >= maximum_cells {
            return Err(DecodeError::Limit {
                collection: "cell-trend packets",
                limit: maximum_cells,
            });
        }
        expect_u16(message, cursor, 21, "cell-trend data packet code")?;
        let length = usize::from(be_u16(
            message,
            cursor + 2,
            "cell-trend data packet length",
        )?);
        if length < 5 {
            return Err(invalid(
                "cell-trend data packet length",
                cursor + 2,
                format!("{length} is shorter than the storm ID/trend header"),
            ));
        }
        let data = checked_slice(message, cursor + 4, length, "cell-trend data packet")?;
        if !data[..2].iter().all(|byte| byte.is_ascii_alphanumeric()) {
            return Err(DecodeError::NonAscii {
                context: "cell-trend storm ID",
                offset: cursor + 4,
            });
        }
        cursor = cursor
            .checked_add(4 + length)
            .ok_or_else(|| invalid("cell-trend packet", cursor, "offset overflow"))?;
        cells += 1;
    }
    Ok(())
}

fn parse_structure_row(line: &str) -> Result<Option<StormStructureCell>, DecodeError> {
    // ROC 2620003AE Appendix C Format V fixes these 80-character columns.
    let bytes = line.as_bytes();
    if bytes.len() < 76 {
        return Ok(None);
    }
    let storm_id = ascii_column(bytes, 5, 7).trim();
    if !valid_storm_id(storm_id) {
        return Ok(None);
    }
    let position_text = ascii_column(bytes, 13, 20).trim();
    if !position_text.contains('/') {
        return Ok(None);
    }
    let position = parse_azimuth_range(position_text, "storm structure position")?;
    let base_kft_agl = parse_height(ascii_column(bytes, 23, 29), "storm base")?;
    let top_kft_agl = parse_height(ascii_column(bytes, 31, 37), "storm top")?;
    let cell_based_vil_kg_m2 = parse_u16(ascii_column(bytes, 43, 49), "cell-based VIL")?;
    let maximum_reflectivity_dbz = parse_u16(ascii_column(bytes, 59, 64), "maximum reflectivity")?;
    let maximum_reflectivity_height_kft_agl =
        parse_f32(ascii_column(bytes, 68, 75), "maximum-reflectivity height")?;
    if cell_based_vil_kg_m2 > 120 {
        return Err(invalid("cell-based VIL", 0, "outside 0..=120 kg/m^2"));
    }
    if maximum_reflectivity_dbz > 95 {
        return Err(invalid("maximum reflectivity", 0, "outside 0..=95 dBZ"));
    }
    if !(0.0..=70.0).contains(&maximum_reflectivity_height_kft_agl) {
        return Err(invalid(
            "maximum-reflectivity height",
            0,
            "outside 0.0..=70.0 kft AGL",
        ));
    }
    Ok(Some(StormStructureCell {
        storm_id: storm_id.to_owned(),
        position,
        base_kft_agl,
        top_kft_agl,
        cell_based_vil_kg_m2,
        maximum_reflectivity_dbz,
        maximum_reflectivity_height_kft_agl,
    }))
}

fn parse_height(text: &str, context: &'static str) -> Result<QualifiedHeight, DecodeError> {
    let trimmed = text.trim();
    let (qualifier, number) = if let Some(rest) = trimmed.strip_prefix('<') {
        (HeightQualifier::BelowLowestElevation, rest)
    } else if let Some(rest) = trimmed.strip_prefix('>') {
        (HeightQualifier::AboveHighestElevation, rest)
    } else {
        (HeightQualifier::Exact, trimmed)
    };
    let kft_agl = parse_f32(number, context)?;
    if !(0.0..=70.0).contains(&kft_agl) {
        return Err(invalid(context, 0, "outside 0.0..=70.0 kft AGL"));
    }
    Ok(QualifiedHeight { kft_agl, qualifier })
}

fn parse_optional_azimuth_range(
    text: &str,
    context: &'static str,
) -> Result<Option<AzimuthRange>, DecodeError> {
    if text.is_empty() || text.eq_ignore_ascii_case("NO DATA") {
        Ok(None)
    } else {
        parse_azimuth_range(text, context).map(Some)
    }
}

fn parse_azimuth_range(text: &str, context: &'static str) -> Result<AzimuthRange, DecodeError> {
    let (azimuth, range) = split_once_required(text, '/', context)?;
    let azimuth_degrees = parse_u16(azimuth, context)?;
    let range_nautical_miles = parse_u16(range, context)?;
    if azimuth_degrees > 360 || range_nautical_miles > 248 {
        return Err(invalid(
            context,
            0,
            format!("{azimuth_degrees}/{range_nautical_miles} outside ROC bounds"),
        ));
    }
    Ok(AzimuthRange {
        azimuth_degrees,
        range_nautical_miles,
    })
}

fn cross_check_azimuth_range(
    exact: RadarRelativePosition,
    rounded: AzimuthRange,
    storm_id: &str,
    _radar: GeographicPoint,
) -> Result<(), DecodeError> {
    let east_km = f64::from(exact.i_quarter_km) * 0.25;
    let north_km = f64::from(exact.j_quarter_km) * 0.25;
    let exact_range_nm = east_km.hypot(north_km) / 1.852;
    let mut exact_azimuth = east_km.atan2(north_km).to_degrees();
    if exact_azimuth < 0.0 {
        exact_azimuth += 360.0;
    }
    let reported_azimuth = if rounded.azimuth_degrees == 360 {
        0.0
    } else {
        f64::from(rounded.azimuth_degrees)
    };
    let azimuth_delta = (exact_azimuth - reported_azimuth)
        .abs()
        .min(360.0 - (exact_azimuth - reported_azimuth).abs());
    if azimuth_delta > 1.1 || (exact_range_nm - f64::from(rounded.range_nautical_miles)).abs() > 1.1
    {
        return Err(DecodeError::CrossCheck(format!(
            "storm {storm_id} exact packet position disagrees with rounded tabular AZ/RAN"
        )));
    }
    Ok(())
}

fn number_before_keyword(line: &str, keyword: &str) -> Option<u16> {
    let prefix = line.get(..line.find(keyword)?)?;
    prefix.split_ascii_whitespace().rev().find_map(|token| {
        token
            .trim_matches(|ch: char| !ch.is_ascii_digit())
            .parse()
            .ok()
    })
}

fn number_after_keyword(line: &str, keyword: &str) -> Option<u16> {
    line.get(line.find(keyword)? + keyword.len()..)?
        .split_ascii_whitespace()
        .find_map(|token| {
            token
                .trim_matches(|ch: char| !ch.is_ascii_digit())
                .parse()
                .ok()
        })
}

fn valid_storm_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 2 && bytes[0].is_ascii_uppercase() && bytes[1].is_ascii_digit()
}

fn ascii_column(bytes: &[u8], start: usize, end: usize) -> &str {
    if start >= bytes.len() {
        return "";
    }
    let actual_end = end.min(bytes.len());
    std::str::from_utf8(&bytes[start..actual_end]).unwrap_or_default()
}

fn split_once_required<'a>(
    text: &'a str,
    delimiter: char,
    context: &'static str,
) -> Result<(&'a str, &'a str), DecodeError> {
    text.split_once(delimiter)
        .ok_or_else(|| invalid(context, 0, format!("missing '{delimiter}'")))
}

fn parse_u16(text: &str, context: &'static str) -> Result<u16, DecodeError> {
    text.trim()
        .parse()
        .map_err(|_| invalid(context, 0, format!("invalid integer '{text}'")))
}

fn parse_f32(text: &str, context: &'static str) -> Result<f32, DecodeError> {
    let value: f32 = text
        .trim()
        .parse()
        .map_err(|_| invalid(context, 0, format!("invalid number '{text}'")))?;
    if !value.is_finite() {
        return Err(invalid(context, 0, "non-finite number"));
    }
    Ok(value)
}

fn halfword_offset(
    message: &[u8],
    field_offset: usize,
    context: &'static str,
) -> Result<Option<usize>, DecodeError> {
    let halfwords = be_u32(message, field_offset, context)?;
    if halfwords == 0 {
        return Ok(None);
    }
    let bytes = usize::try_from(halfwords)
        .ok()
        .and_then(|value| value.checked_mul(2))
        .ok_or_else(|| invalid(context, field_offset, "halfword offset overflow"))?;
    if bytes >= message.len() {
        return Err(invalid(
            context,
            field_offset,
            format!(
                "byte offset {bytes} is outside message length {}",
                message.len()
            ),
        ));
    }
    Ok(Some(bytes))
}

fn raw_halfword_offset(
    message: &[u8],
    field_offset: usize,
    context: &'static str,
) -> Result<Option<usize>, DecodeError> {
    let halfwords = be_u32(message, field_offset, context)?;
    if halfwords == 0 {
        return Ok(None);
    }
    usize::try_from(halfwords)
        .ok()
        .and_then(|value| value.checked_mul(2))
        .map(Some)
        .ok_or_else(|| invalid(context, field_offset, "halfword offset overflow"))
}

fn invalid(context: &'static str, offset: usize, detail: impl Into<String>) -> DecodeError {
    DecodeError::Invalid {
        context,
        offset,
        detail: detail.into(),
    }
}

fn block_envelope_end(
    message: &[u8],
    offset: usize,
    expected_id: u16,
    context: &'static str,
) -> Result<usize, DecodeError> {
    expect_i16(message, offset, -1, context)?;
    expect_u16(message, offset + 2, expected_id, context)?;
    let length = usize::try_from(be_u32(message, offset + 4, context)?)
        .map_err(|_| invalid(context, offset + 4, "block length does not fit usize"))?;
    if length < 8 {
        return Err(invalid(
            context,
            offset + 4,
            format!("block length {length} is shorter than its header"),
        ));
    }
    checked_end(message, offset, length, context)
}

fn checked_end(
    data: &[u8],
    start: usize,
    length: usize,
    context: &'static str,
) -> Result<usize, DecodeError> {
    checked_end_bounded(data, start, length, data.len(), context)
}

fn checked_end_bounded(
    data: &[u8],
    start: usize,
    length: usize,
    boundary: usize,
    context: &'static str,
) -> Result<usize, DecodeError> {
    let end = start
        .checked_add(length)
        .ok_or_else(|| invalid(context, start, "length overflow"))?;
    if boundary > data.len() || end > boundary {
        return Err(DecodeError::Truncated {
            context,
            offset: start,
            needed: length,
            available: boundary.min(data.len()).saturating_sub(start),
        });
    }
    Ok(end)
}

fn checked_slice<'a>(
    data: &'a [u8],
    start: usize,
    length: usize,
    context: &'static str,
) -> Result<&'a [u8], DecodeError> {
    checked_slice_bounded(data, start, length, data.len(), context)
}

fn checked_slice_bounded<'a>(
    data: &'a [u8],
    start: usize,
    length: usize,
    boundary: usize,
    context: &'static str,
) -> Result<&'a [u8], DecodeError> {
    let end = checked_end_bounded(data, start, length, boundary, context)?;
    Ok(&data[start..end])
}

fn be_i16(data: &[u8], offset: usize, context: &'static str) -> Result<i16, DecodeError> {
    let bytes = checked_slice(data, offset, 2, context)?;
    Ok(i16::from_be_bytes([bytes[0], bytes[1]]))
}

fn be_u16(data: &[u8], offset: usize, context: &'static str) -> Result<u16, DecodeError> {
    let bytes = checked_slice(data, offset, 2, context)?;
    Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn be_i32(data: &[u8], offset: usize, context: &'static str) -> Result<i32, DecodeError> {
    let bytes = checked_slice(data, offset, 4, context)?;
    Ok(i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn be_u32(data: &[u8], offset: usize, context: &'static str) -> Result<u32, DecodeError> {
    let bytes = checked_slice(data, offset, 4, context)?;
    Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn be_i16_bounded(
    data: &[u8],
    offset: usize,
    context: &'static str,
    boundary: usize,
) -> Result<i16, DecodeError> {
    let bytes = checked_slice_bounded(data, offset, 2, boundary, context)?;
    Ok(i16::from_be_bytes([bytes[0], bytes[1]]))
}

fn be_u16_bounded(
    data: &[u8],
    offset: usize,
    context: &'static str,
    boundary: usize,
) -> Result<u16, DecodeError> {
    let bytes = checked_slice_bounded(data, offset, 2, boundary, context)?;
    Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn be_u32_bounded(
    data: &[u8],
    offset: usize,
    context: &'static str,
    boundary: usize,
) -> Result<u32, DecodeError> {
    let bytes = checked_slice_bounded(data, offset, 4, boundary, context)?;
    Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn expect_i16(
    data: &[u8],
    offset: usize,
    expected: i16,
    context: &'static str,
) -> Result<(), DecodeError> {
    let actual = be_i16(data, offset, context)?;
    if actual == expected {
        Ok(())
    } else {
        Err(invalid(
            context,
            offset,
            format!("expected {expected}, got {actual}"),
        ))
    }
}

fn expect_u16(
    data: &[u8],
    offset: usize,
    expected: u16,
    context: &'static str,
) -> Result<(), DecodeError> {
    let actual = be_u16(data, offset, context)?;
    if actual == expected {
        Ok(())
    } else {
        Err(invalid(
            context,
            offset,
            format!("expected {expected}, got {actual}"),
        ))
    }
}

fn expect_i16_bounded(
    data: &[u8],
    offset: usize,
    expected: i16,
    context: &'static str,
    boundary: usize,
) -> Result<(), DecodeError> {
    let actual = be_i16_bounded(data, offset, context, boundary)?;
    if actual == expected {
        Ok(())
    } else {
        Err(invalid(
            context,
            offset,
            format!("expected {expected}, got {actual}"),
        ))
    }
}

fn local_u16(
    data: &[u8],
    offset: usize,
    absolute_offset: usize,
    context: &'static str,
) -> Result<u16, DecodeError> {
    let bytes =
        data.get(offset..offset.saturating_add(2))
            .ok_or_else(|| DecodeError::Truncated {
                context,
                offset: absolute_offset.saturating_add(offset),
                needed: 2,
                available: data.len().saturating_sub(offset),
            })?;
    Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn slice_i16(data: &[u8], offset: usize) -> i16 {
    // Callers first validate exact packet lengths, so these fixed two-byte
    // fields are provably present.
    i16::from_be_bytes([data[offset], data[offset + 1]])
}
