use super::parser::{DataRepresentation, Grib2Message};

/// Bit reader for extracting packed values from GRIB2 data sections.
pub struct BitReader<'a> {
    data: &'a [u8],
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        BitReader { data, bit_pos: 0 }
    }

    /// Read `n` bits as an unsigned integer (up to 64 bits).
    ///
    /// Optimized to read whole bytes at a time when possible, falling back
    /// to bit-by-bit only for partial-byte boundaries.
    pub fn read_bits(&mut self, n: usize) -> u64 {
        if n == 0 {
            return 0;
        }

        let byte_offset = self.bit_pos / 8;
        let bit_offset = self.bit_pos % 8;

        // Fast path: all bits fit within available bytes and n <= 56
        // We can grab up to 8 bytes from the data, shift and mask.
        if n <= 56 && byte_offset + (bit_offset + n + 7) / 8 <= self.data.len() {
            // Read up to 8 bytes starting at byte_offset into a u64 (big-endian)
            let bytes_needed = (bit_offset + n + 7) / 8;
            let mut raw: u64 = 0;
            for i in 0..bytes_needed {
                raw = (raw << 8) | self.data[byte_offset + i] as u64;
            }
            // The bits we want start at bit_offset from the MSB of the first byte
            // and we read `bytes_needed * 8` bits total. We need bits
            // [bit_offset .. bit_offset + n] counting from the MSB.
            let shift = bytes_needed * 8 - bit_offset - n;
            let mask = (1u64 << n) - 1;
            self.bit_pos += n;
            return (raw >> shift) & mask;
        }

        // Slow path: bit-by-bit (for edge cases or very large reads)
        let mut result: u64 = 0;
        for _ in 0..n {
            let bi = self.bit_pos / 8;
            let bit_idx = 7 - (self.bit_pos % 8);
            if bi < self.data.len() {
                result = (result << 1) | ((self.data[bi] >> bit_idx) as u64 & 1);
            } else {
                result <<= 1;
            }
            self.bit_pos += 1;
        }
        result
    }

    /// Read `n` bits as a signed integer using sign-magnitude convention.
    /// MSB = sign (1 = negative), remaining bits = magnitude.
    pub fn read_signed_bits(&mut self, n: usize) -> i64 {
        if n == 0 {
            return 0;
        }
        if n == 1 {
            let _bit = self.read_bits(1);
            return 0;
        }
        let sign = self.read_bits(1);
        let magnitude = self.read_bits(n - 1) as i64;
        if sign == 1 {
            -magnitude
        } else {
            magnitude
        }
    }

    fn read_bits_checked(&mut self, n: usize, label: &str) -> Result<u64, String> {
        validate_bit_width(n, label)?;
        if self.remaining_bits() < n {
            return Err(format!(
                "truncated {label}: need {n} bits, have {}",
                self.remaining_bits()
            ));
        }
        Ok(self.read_bits(n))
    }

    fn read_signed_bits_checked(&mut self, n: usize, label: &str) -> Result<i64, String> {
        let encoded = self.read_bits_checked(n, label)?;
        if n == 0 {
            return Ok(0);
        }
        let sign_mask = 1_u64 << (n - 1);
        let magnitude = encoded & (sign_mask - 1);
        let magnitude =
            i64::try_from(magnitude).map_err(|_| format!("{label} magnitude exceeded i64"))?;
        Ok(if encoded & sign_mask == 0 {
            magnitude
        } else {
            -magnitude
        })
    }

    /// Align the bit position to the next byte boundary.
    pub fn align_to_byte(&mut self) {
        let rem = self.bit_pos % 8;
        if rem != 0 {
            self.bit_pos += 8 - rem;
        }
    }

    /// Number of bits remaining.
    pub fn remaining_bits(&self) -> usize {
        let total = self.data.len() * 8;
        if self.bit_pos >= total {
            0
        } else {
            total - self.bit_pos
        }
    }
}

/// Unpack a GRIB2 message's data section to floating-point values.
pub fn unpack_message(msg: &Grib2Message) -> crate::error::Result<Vec<f64>> {
    let dr = &msg.data_rep;

    let num_points = msg.grid.nx as usize * msg.grid.ny as usize;
    let values = match dr.template {
        0 => unpack_simple(&msg.raw_data, dr).map_err(crate::RustmetError::Unpack)?,
        2 => unpack_complex(&msg.raw_data, dr).map_err(crate::RustmetError::Unpack)?,
        3 => unpack_complex_spatial(&msg.raw_data, dr).map_err(crate::RustmetError::Unpack)?,
        4 => unpack_ieee(&msg.raw_data, dr).map_err(crate::RustmetError::Unpack)?,
        40 => unpack_jpeg2000(&msg.raw_data, dr).map_err(crate::RustmetError::Unpack)?,
        41 => unpack_png(&msg.raw_data, dr).map_err(crate::RustmetError::Unpack)?,
        42 => unpack_ccsds(&msg.raw_data, dr).map_err(crate::RustmetError::Unpack)?,
        50 => unpack_spectral_simple(&msg.raw_data, dr).map_err(crate::RustmetError::Unpack)?,
        51 => unpack_spectral_complex(&msg.raw_data, dr).map_err(crate::RustmetError::Unpack)?,
        61 => unpack_simple_log(&msg.raw_data, dr).map_err(crate::RustmetError::Unpack)?,
        200 => unpack_rle(&msg.raw_data, dr, num_points).map_err(crate::RustmetError::Unpack)?,
        _ => {
            return Err(crate::RustmetError::UnsupportedTemplate {
                template: dr.template,
                detail: "data representation template".to_string(),
            })
        }
    };

    let values = if dr.bits_per_value == 0 && values.len() <= 1 && num_points > 1 {
        let fill = values.first().copied().unwrap_or(dr.reference_value as f64);
        vec![fill; num_points]
    } else {
        values
    };

    if matches!(dr.template, 2 | 3) {
        let active_points = msg
            .bitmap
            .as_ref()
            .map(|bitmap| bitmap.iter().filter(|present| **present).count())
            .unwrap_or(num_points);
        let declared = dr.section5_num_data_points as usize;
        let expected = if declared == 0 {
            active_points
        } else if declared == active_points {
            declared
        } else {
            return Err(crate::RustmetError::Unpack(format!(
                "Section 5 declares {declared} coded values, but the grid/bitmap requires {active_points}"
            )));
        };
        if values.len() != expected {
            return Err(crate::RustmetError::Unpack(format!(
                "complex packing decoded {} values, expected {expected}",
                values.len()
            )));
        }
    }

    // Apply bitmap if present
    let values = if let Some(ref bitmap) = msg.bitmap {
        let n = bitmap.len();
        let mut result = vec![f64::NAN; n];
        let mut val_idx = 0;
        for i in 0..n {
            if bitmap[i] {
                if val_idx < values.len() {
                    result[i] = values[val_idx];
                    val_idx += 1;
                }
            }
        }
        result
    } else {
        values
    };

    // Preserve original scan order by default (matches ecCodes behavior).
    // Users who need north-to-south (top-down) row order for rendering
    // should call `flip_scan_order()` on the values.

    Ok(values)
}

/// Flip rows of a 2D field to convert between scan orders.
///
/// GRIB2 scan_mode bit 6 (0x40) indicates +j direction (south-to-north).
/// This function reverses the row order, converting south-to-north to
/// north-to-south (top-down) or vice versa.
///
/// Call this after `unpack_message` if you need north-to-south order
/// for rendering/display and the message has `scan_mode & 0x40 != 0`.
pub fn flip_rows(values: &mut [f64], nx: usize, ny: usize) {
    if nx == 0 || ny == 0 || values.len() != nx * ny {
        return;
    }
    for j in 0..ny / 2 {
        let j_rev = ny - 1 - j;
        let (top, bot) = values.split_at_mut(j_rev * nx);
        let top_row = &mut top[j * nx..j * nx + nx];
        let bot_row = &mut bot[..nx];
        top_row.swap_with_slice(bot_row);
    }
}

/// Unpack a message and normalize to north-to-south row order.
///
/// This is a convenience that calls `unpack_message` then `flip_rows`
/// if scan_mode bit 6 is set. Useful for rendering pipelines.
pub fn unpack_message_normalized(msg: &Grib2Message) -> crate::error::Result<Vec<f64>> {
    let mut values = unpack_message(msg)?;
    if msg.grid.scan_mode & 0x40 != 0 {
        flip_rows(&mut values, msg.grid.nx as usize, msg.grid.ny as usize);
    }
    Ok(values)
}

/// Apply the GRIB2 scaling formula: Y = (R + X * 2^E) * 10^(-D)
fn apply_scaling(raw: &[i64], dr: &DataRepresentation) -> Vec<f64> {
    let r = dr.reference_value as f64;
    let e = dr.binary_scale as f64;
    let d = dr.decimal_scale as f64;
    let two_e = 2.0_f64.powf(e);
    let ten_neg_d = 10.0_f64.powf(-d);

    raw.iter()
        .map(|&x| (r + x as f64 * two_e) * ten_neg_d)
        .collect()
}

fn scale_raw_value(raw: i64, dr: &DataRepresentation) -> f64 {
    let r = dr.reference_value as f64;
    let two_e = 2.0_f64.powi(dr.binary_scale as i32);
    let ten_neg_d = 10.0_f64.powi(-(dr.decimal_scale as i32));
    (r + raw as f64 * two_e) * ten_neg_d
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PackedComplexValue {
    Present(i64),
    PrimaryMissing,
    SecondaryMissing,
}

struct ComplexGroups<'a> {
    reader: BitReader<'a>,
    group_refs: Vec<u64>,
    group_widths: Vec<usize>,
    group_lengths: Vec<usize>,
    group_idx: usize,
    value_idx_in_group: usize,
    total_values: usize,
    missing_value_management: u8,
    group_reference_bits: usize,
}

impl<'a> ComplexGroups<'a> {
    fn new(
        data: &'a [u8],
        dr: &DataRepresentation,
        expected_values: Option<usize>,
    ) -> Result<Self, String> {
        let ng = dr.num_groups as usize;
        if dr.missing_value_management > 2 {
            return Err(format!(
                "unsupported complex-packing missing-value management {}",
                dr.missing_value_management
            ));
        }
        if ng == 0 {
            return Ok(Self {
                reader: BitReader::new(data),
                group_refs: Vec::new(),
                group_widths: Vec::new(),
                group_lengths: Vec::new(),
                group_idx: 0,
                value_idx_in_group: 0,
                total_values: 0,
                missing_value_management: dr.missing_value_management,
                group_reference_bits: dr.bits_per_value as usize,
            });
        }

        let mut reader = BitReader::new(data);
        let bpv = dr.bits_per_value as usize;
        validate_bit_width(bpv, "group reference")?;
        let mut group_refs = Vec::with_capacity(ng);
        for _ in 0..ng {
            group_refs.push(reader.read_bits_checked(bpv, "group references")?);
        }
        reader.align_to_byte();

        let gwb = dr.group_width_bits as usize;
        validate_bit_width(gwb, "scaled group width")?;
        let mut group_widths = Vec::with_capacity(ng);
        for _ in 0..ng {
            let encoded = usize::try_from(reader.read_bits_checked(gwb, "group widths")?)
                .map_err(|_| "encoded group width did not fit usize".to_string())?;
            let width = encoded
                .checked_add(dr.group_width_ref as usize)
                .ok_or_else(|| "complex-packing group width overflow".to_string())?;
            validate_bit_width(width, "group value")?;
            group_widths.push(width);
        }
        reader.align_to_byte();

        let glb = dr.group_length_bits as usize;
        validate_bit_width(glb, "scaled group length")?;
        let mut group_lengths = Vec::with_capacity(ng);
        for _ in 0..ng {
            let stored = usize::try_from(reader.read_bits_checked(glb, "group lengths")?)
                .map_err(|_| "encoded group length did not fit usize".to_string())?;
            let length = stored
                .checked_mul(dr.group_length_inc as usize)
                .and_then(|value| value.checked_add(dr.group_length_ref as usize))
                .ok_or_else(|| "complex-packing group length overflow".to_string())?;
            group_lengths.push(length);
        }
        group_lengths[ng - 1] = dr.last_group_length as usize;
        reader.align_to_byte();

        let total_values = group_lengths.iter().try_fold(0usize, |total, length| {
            total
                .checked_add(*length)
                .ok_or_else(|| "complex-packing value count overflow".to_string())
        })?;
        if let Some(expected) = expected_values {
            if total_values != expected {
                return Err(format!(
                    "complex-packing group lengths describe {total_values} values, expected {expected}"
                ));
            }
        }
        let required_value_bits = group_widths.iter().zip(&group_lengths).try_fold(
            0usize,
            |total, (width, length)| {
                width
                    .checked_mul(*length)
                    .and_then(|bits| total.checked_add(bits))
                    .ok_or_else(|| "complex-packing data bit count overflow".to_string())
            },
        )?;
        if reader.remaining_bits() < required_value_bits {
            return Err(format!(
                "complex-packing data truncated: need {required_value_bits} value bits, have {}",
                reader.remaining_bits()
            ));
        }

        Ok(Self {
            reader,
            group_refs,
            group_widths,
            group_lengths,
            group_idx: 0,
            value_idx_in_group: 0,
            total_values,
            missing_value_management: dr.missing_value_management,
            group_reference_bits: bpv,
        })
    }

    fn total_values(&self) -> usize {
        self.total_values
    }

    fn next_packed(&mut self) -> Result<Option<PackedComplexValue>, String> {
        while self.group_idx < self.group_lengths.len()
            && self.value_idx_in_group >= self.group_lengths[self.group_idx]
        {
            self.group_idx += 1;
            self.value_idx_in_group = 0;
        }
        if self.group_idx >= self.group_lengths.len() {
            return Ok(None);
        }

        let width = self.group_widths[self.group_idx];
        let gref = self.group_refs[self.group_idx];
        self.value_idx_in_group += 1;
        if width == 0 {
            if let Some(missing) = classify_missing_code(
                gref,
                self.group_reference_bits,
                self.missing_value_management,
            ) {
                Ok(Some(missing))
            } else {
                let value = i64::try_from(gref)
                    .map_err(|_| "complex-packing group reference exceeded i64".to_string())?;
                Ok(Some(PackedComplexValue::Present(value)))
            }
        } else {
            let encoded = self.reader.read_bits_checked(width, "group values")?;
            if let Some(missing) =
                classify_missing_code(encoded, width, self.missing_value_management)
            {
                Ok(Some(missing))
            } else {
                let value = gref
                    .checked_add(encoded)
                    .ok_or_else(|| "complex-packing group value overflow".to_string())?;
                let value = i64::try_from(value)
                    .map_err(|_| "complex-packing group value exceeded i64".to_string())?;
                Ok(Some(PackedComplexValue::Present(value)))
            }
        }
    }
}

fn validate_bit_width(bits: usize, label: &str) -> Result<(), String> {
    if bits > 64 {
        Err(format!("{label} bit width {bits} exceeds 64"))
    } else {
        Ok(())
    }
}

fn all_ones(bits: usize) -> u64 {
    match bits {
        0 => 0,
        64 => u64::MAX,
        _ => (1_u64 << bits) - 1,
    }
}

fn classify_missing_code(encoded: u64, bits: usize, management: u8) -> Option<PackedComplexValue> {
    if management == 0 {
        return None;
    }
    let primary = all_ones(bits);
    if encoded == primary {
        return Some(PackedComplexValue::PrimaryMissing);
    }
    if management == 2 && primary.checked_sub(1) == Some(encoded) {
        return Some(PackedComplexValue::SecondaryMissing);
    }
    None
}

fn packed_value_to_f64(packed: PackedComplexValue, dr: &DataRepresentation) -> Result<f64, String> {
    match packed {
        PackedComplexValue::Present(value) => Ok(scale_raw_value(value, dr)),
        // Match bitmap-missing behavior: explicit primary/secondary codes
        // become NaN, while their raw Section 5 substitutes remain available
        // as metadata rather than entering meteorological arithmetic.
        PackedComplexValue::PrimaryMissing | PackedComplexValue::SecondaryMissing => Ok(f64::NAN),
    }
}

struct SpatialDifferencingDecoder<'a> {
    groups: ComplexGroups<'a>,
    dr: &'a DataRepresentation,
    order: usize,
    initial_values: Vec<i64>,
    minimum: i64,
    nonmissing_index: usize,
    previous: Option<i64>,
    previous_previous: Option<i64>,
}

impl<'a> SpatialDifferencingDecoder<'a> {
    fn new(
        data: &'a [u8],
        dr: &'a DataRepresentation,
        expected_values: Option<usize>,
    ) -> Result<Self, String> {
        let order = dr.spatial_diff_order as usize;
        if !matches!(order, 1 | 2) {
            return Err(format!(
                "unsupported spatial-differencing order {order}; expected 1 or 2"
            ));
        }
        let nbits = (dr.spatial_diff_bytes as usize)
            .checked_mul(8)
            .ok_or_else(|| "spatial-differencing descriptor width overflow".to_string())?;
        validate_bit_width(nbits, "spatial-differencing descriptor")?;
        let mut reader = BitReader::new(data);
        let mut initial_values = Vec::with_capacity(order);
        if nbits == 0 {
            initial_values.resize(order, 0);
        } else {
            for _ in 0..order {
                let value = reader.read_bits_checked(nbits, "spatial initial values")?;
                initial_values.push(
                    i64::try_from(value).map_err(|_| {
                        "spatial-differencing initial value exceeded i64".to_string()
                    })?,
                );
            }
        }
        let minimum = if nbits == 0 {
            0
        } else {
            reader.read_signed_bits_checked(nbits, "spatial minimum")?
        };
        reader.align_to_byte();
        let consumed_bytes = (reader.bit_pos + 7) / 8;
        let remaining = data
            .get(consumed_bytes..)
            .ok_or_else(|| "spatial-differencing header exceeded data length".to_string())?;
        let groups = ComplexGroups::new(remaining, dr, expected_values)?;
        Ok(Self {
            groups,
            dr,
            order,
            initial_values,
            minimum,
            nonmissing_index: 0,
            previous: None,
            previous_previous: None,
        })
    }

    fn total_values(&self) -> usize {
        self.groups.total_values()
    }

    fn next_scaled(&mut self) -> Result<Option<f64>, String> {
        let Some(packed) = self.groups.next_packed()? else {
            return Ok(None);
        };
        let raw = match packed {
            PackedComplexValue::PrimaryMissing => {
                return Ok(Some(f64::NAN));
            }
            PackedComplexValue::SecondaryMissing => {
                return Ok(Some(f64::NAN));
            }
            PackedComplexValue::Present(raw) => raw,
        };

        let reconstructed = if self.nonmissing_index < self.order {
            self.initial_values[self.nonmissing_index]
        } else {
            let difference = raw.checked_add(self.minimum).ok_or_else(|| {
                "spatial-differencing difference overflow while adding minimum".to_string()
            })?;
            match self.order {
                1 => difference
                    .checked_add(self.previous.expect("first-order history initialized"))
                    .ok_or_else(|| "first-order spatial recurrence overflow".to_string())?,
                2 => {
                    let previous = self.previous.expect("second-order history initialized");
                    let previous_previous = self
                        .previous_previous
                        .expect("second-order history initialized");
                    previous
                        .checked_mul(2)
                        .and_then(|value| value.checked_sub(previous_previous))
                        .and_then(|value| value.checked_add(difference))
                        .ok_or_else(|| "second-order spatial recurrence overflow".to_string())?
                }
                _ => unreachable!(),
            }
        };

        self.nonmissing_index += 1;
        self.previous_previous = self.previous;
        self.previous = Some(reconstructed);
        Ok(Some(scale_raw_value(reconstructed, self.dr)))
    }
}

/// Template 5.0: Simple packing.
fn unpack_simple(data: &[u8], dr: &DataRepresentation) -> Result<Vec<f64>, String> {
    let bpv = dr.bits_per_value as usize;
    if bpv == 0 {
        // All values are the reference value
        let n = if !data.is_empty() { 1 } else { 0 };
        return Ok(vec![dr.reference_value as f64; n]);
    }

    let total_bits = data.len() * 8;
    let n = total_bits / bpv;
    let mut reader = BitReader::new(data);
    let mut raw = Vec::with_capacity(n);
    for _ in 0..n {
        raw.push(reader.read_bits(bpv) as i64);
    }

    Ok(apply_scaling(&raw, dr))
}

/// Template 5.2: Complex packing.
fn unpack_complex(data: &[u8], dr: &DataRepresentation) -> Result<Vec<f64>, String> {
    let expected = (dr.template == 2 && dr.section5_num_data_points != 0)
        .then_some(dr.section5_num_data_points as usize);
    let mut groups = ComplexGroups::new(data, dr, expected)?;
    let mut values = Vec::with_capacity(groups.total_values());
    while let Some(packed) = groups.next_packed()? {
        values.push(packed_value_to_f64(packed, dr)?);
    }
    Ok(values)
}

/// Template 5.3: Complex packing with spatial differencing.
fn unpack_complex_spatial(data: &[u8], dr: &DataRepresentation) -> Result<Vec<f64>, String> {
    if dr.spatial_diff_order == 0 {
        return unpack_complex(data, dr);
    }
    let expected =
        (dr.section5_num_data_points != 0).then_some(dr.section5_num_data_points as usize);
    let mut decoder = SpatialDifferencingDecoder::new(data, dr, expected)?;
    let mut values = Vec::with_capacity(decoder.total_values());
    while let Some(value) = decoder.next_scaled()? {
        values.push(value);
    }
    Ok(values)
}

/// Template 5.40: JPEG2000 packing (stub for platforms without openjp2).
#[cfg(not(feature = "jpeg2000"))]
fn unpack_jpeg2000(_data: &[u8], _dr: &DataRepresentation) -> Result<Vec<f64>, String> {
    Err("JPEG2000 decoding not available (openjp2 feature disabled)".into())
}

/// Template 5.40: JPEG2000 packing.
///
/// Uses openjp2's C-style API to decode a JPEG2000 codestream embedded in GRIB2 Section 7.
/// GRIB2 JPEG2000 data is always a raw J2K codestream (starts with FF 4F).
#[cfg(feature = "jpeg2000")]
fn unpack_jpeg2000(data: &[u8], dr: &DataRepresentation) -> Result<Vec<f64>, String> {
    use openjp2::openjpeg::*;
    use std::ffi::c_void;

    if data.is_empty() {
        return Ok(Vec::new());
    }

    // Detect format - GRIB2 embeds raw J2K codestreams (0xFF 0x4F)
    let format = openjp2::detect_format(data)
        .map_err(|e| format!("JPEG2000 format detection failed: {}", e))?;
    let codec_format = match format {
        openjp2::J2KFormat::J2K => OPJ_CODEC_J2K,
        openjp2::J2KFormat::JP2 => OPJ_CODEC_JP2,
        openjp2::J2KFormat::JPT => OPJ_CODEC_JPT,
    };

    // We use the C-style FFI API because the Rust Stream type has no public
    // constructor for in-memory buffers.

    // WrappedSlice for the stream callbacks
    struct WrappedSlice {
        offset: usize,
        len: usize,
        ptr: *const u8,
    }

    extern "C" fn j2k_read(p_buffer: *mut c_void, nb_bytes: usize, p_data: *mut c_void) -> usize {
        if p_buffer.is_null() || nb_bytes == 0 {
            return usize::MAX;
        }
        let slice = unsafe { &mut *(p_data as *mut WrappedSlice) };
        let remaining = slice.len - slice.offset;
        if remaining == 0 {
            return usize::MAX;
        }
        let n = remaining.min(nb_bytes);
        unsafe {
            std::ptr::copy_nonoverlapping(slice.ptr.add(slice.offset), p_buffer as *mut u8, n);
        }
        slice.offset += n;
        n
    }

    extern "C" fn j2k_skip(nb_bytes: i64, p_data: *mut c_void) -> i64 {
        let slice = unsafe { &mut *(p_data as *mut WrappedSlice) };
        let new_off = (slice.offset as i64 + nb_bytes).max(0) as usize;
        slice.offset = new_off.min(slice.len);
        nb_bytes
    }

    extern "C" fn j2k_seek(nb_bytes: i64, p_data: *mut c_void) -> i32 {
        let slice = unsafe { &mut *(p_data as *mut WrappedSlice) };
        let off = nb_bytes as usize;
        if off <= slice.len {
            slice.offset = off;
            1
        } else {
            0
        }
    }

    extern "C" fn j2k_free(p_data: *mut c_void) {
        drop(unsafe { Box::from_raw(p_data as *mut WrappedSlice) });
    }

    let data_len = data.len();
    let wrapped = Box::new(WrappedSlice {
        offset: 0,
        len: data_len,
        ptr: data.as_ptr(),
    });
    let p_data = Box::into_raw(wrapped) as *mut c_void;

    // Create stream
    let stream = unsafe {
        let s = opj_stream_default_create(1);
        if s.is_null() {
            // Clean up wrapped data
            drop(Box::from_raw(p_data as *mut WrappedSlice));
            return Err("Failed to create JPEG2000 stream".into());
        }
        opj_stream_set_read_function(s, Some(j2k_read));
        opj_stream_set_skip_function(s, Some(j2k_skip));
        opj_stream_set_seek_function(s, Some(j2k_seek));
        opj_stream_set_user_data_length(s, data_len as u64);
        opj_stream_set_user_data(s, p_data, Some(j2k_free));
        s
    };

    // Create codec
    let codec = opj_create_decompress(codec_format);
    if codec.is_null() {
        unsafe {
            opj_stream_destroy(stream);
        }
        return Err("Failed to create JPEG2000 decoder".into());
    }

    // Setup decoder
    let mut params = opj_dparameters_t::default();
    let ret = unsafe { opj_setup_decoder(codec, &mut params) };
    if ret == 0 {
        unsafe {
            opj_destroy_codec(codec);
            opj_stream_destroy(stream);
        }
        return Err("Failed to setup JPEG2000 decoder".into());
    }

    // Read header
    let mut image: *mut opj_image_t = std::ptr::null_mut();
    let ret = unsafe { opj_read_header(stream, codec, &mut image) };
    if ret == 0 || image.is_null() {
        unsafe {
            opj_destroy_codec(codec);
            opj_stream_destroy(stream);
            if !image.is_null() {
                opj_image_destroy(image);
            }
        }
        return Err("Failed to read JPEG2000 header".into());
    }

    // Decode
    let ret = unsafe { opj_decode(codec, stream, image) };
    if ret == 0 {
        unsafe {
            opj_destroy_codec(codec);
            opj_stream_destroy(stream);
            opj_image_destroy(image);
        }
        return Err("JPEG2000 decode failed".into());
    }

    // End decompress
    unsafe {
        opj_end_decompress(codec, stream);
    }

    // Extract data from first component
    let img = unsafe { &*image };
    let numcomps = img.numcomps as usize;
    if numcomps == 0 || img.comps.is_null() {
        unsafe {
            opj_destroy_codec(codec);
            opj_stream_destroy(stream);
            opj_image_destroy(image);
        }
        return Err("JPEG2000 image has no components".into());
    }

    let comp = unsafe { &*img.comps };
    let n = (comp.w * comp.h) as usize;
    let raw: Vec<i64> = if let Some(data_slice) = comp.data() {
        data_slice.iter().take(n).map(|&v| v as i64).collect()
    } else {
        unsafe {
            opj_destroy_codec(codec);
            opj_stream_destroy(stream);
            opj_image_destroy(image);
        }
        return Err("JPEG2000 component has no data".into());
    };

    // Cleanup
    unsafe {
        opj_destroy_codec(codec);
        opj_stream_destroy(stream);
        opj_image_destroy(image);
    }

    Ok(apply_scaling(&raw, dr))
}

/// Template 5.41: PNG packing.
fn unpack_png(data: &[u8], dr: &DataRepresentation) -> Result<Vec<f64>, String> {
    if data.is_empty() {
        return Ok(Vec::new());
    }

    let decoder = png::Decoder::new(std::io::Cursor::new(data));
    let mut reader = decoder
        .read_info()
        .map_err(|e| format!("PNG decode error: {}", e))?;

    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| format!("PNG frame error: {}", e))?;
    let bytes = &buf[..info.buffer_size()];

    let bpv = dr.bits_per_value as usize;
    let mut raw = Vec::new();

    match bpv {
        8 => {
            for &b in bytes {
                raw.push(b as i64);
            }
        }
        16 => {
            for chunk in bytes.chunks_exact(2) {
                raw.push(u16::from_be_bytes([chunk[0], chunk[1]]) as i64);
            }
        }
        24 => {
            for chunk in bytes.chunks_exact(3) {
                let v = ((chunk[0] as u32) << 16) | ((chunk[1] as u32) << 8) | chunk[2] as u32;
                raw.push(v as i64);
            }
        }
        32 => {
            for chunk in bytes.chunks_exact(4) {
                raw.push(u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) as i64);
            }
        }
        _ => {
            for &b in bytes {
                raw.push(b as i64);
            }
        }
    }

    Ok(apply_scaling(&raw, dr))
}

/// Template 5.4 / 7.4: IEEE Floating Point packing.
///
/// Values are stored as IEEE 754 floats directly — no packing or scaling.
/// Supports 32-bit (f32) and 64-bit (f64) precision based on `bits_per_value`.
fn unpack_ieee(data: &[u8], dr: &DataRepresentation) -> Result<Vec<f64>, String> {
    if data.is_empty() {
        return Ok(Vec::new());
    }

    let bpv = dr.bits_per_value as usize;
    match bpv {
        32 => {
            let n = data.len() / 4;
            let mut values = Vec::with_capacity(n);
            for chunk in data.chunks_exact(4) {
                let v = f32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                values.push(v as f64);
            }
            Ok(values)
        }
        64 => {
            let n = data.len() / 8;
            let mut values = Vec::with_capacity(n);
            for chunk in data.chunks_exact(8) {
                let v = f64::from_be_bytes([
                    chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
                ]);
                values.push(v);
            }
            Ok(values)
        }
        _ => Err(format!(
            "IEEE float packing: unsupported bits_per_value={} (expected 32 or 64)",
            bpv
        )),
    }
}

/// Template 5.42 / 7.42: CCSDS (AEC/SZIP) packing.
///
/// This template uses the CCSDS Adaptive Entropy Coding (AEC) algorithm.
/// A full implementation requires the `libaec` library. For now, this returns
/// a clear error message indicating the dependency is needed.
fn unpack_ccsds(_data: &[u8], dr: &DataRepresentation) -> Result<Vec<f64>, String> {
    Err(format!(
        "CCSDS/AEC decoding (template 5.42) is not yet supported. \
         This data uses CCSDS flags=0x{:04X}, block_size={}, rsi={}. \
         A libaec/AEC decoder integration is required.",
        dr.ccsds_flags, dr.ccsds_block_size, dr.ccsds_rsi
    ))
}

/// Template 5.61 / 7.61: Simple packing with logarithm pre-processing.
///
/// Values were stored as log10(value + 1) before simple packing.
/// We unpack using simple packing, then reverse the transform: value = 10^x - 1.
fn unpack_simple_log(data: &[u8], dr: &DataRepresentation) -> Result<Vec<f64>, String> {
    let packed = unpack_simple(data, dr)?;
    Ok(packed.iter().map(|&v| 10f64.powf(v) - 1.0).collect())
}

/// Template 5.200 / 7.200: Run Length Encoding (NCEP local extension).
///
/// Used for categorical/discrete data (e.g., precipitation type, soil type).
/// Data is encoded as run-length encoded values where each entry consists of
/// a level value and a run length packed into `bits_per_value` bits.
///
/// The encoding uses a variable-length scheme where the most significant bit(s)
/// determine the level value and the remaining bits encode the run count.
fn unpack_rle(data: &[u8], dr: &DataRepresentation, num_points: usize) -> Result<Vec<f64>, String> {
    if data.is_empty() {
        return Ok(vec![0.0; num_points]);
    }

    let bpv = dr.bits_per_value as usize;
    if bpv == 0 {
        return Ok(vec![dr.reference_value as f64; num_points]);
    }

    let mut reader = BitReader::new(data);
    let mut result = Vec::with_capacity(num_points);

    // NCEP RLE encoding for template 5.200:
    // Read alternating (value, count) pairs, each `bpv` bits wide.
    // The value is the category/level, count is how many consecutive
    // grid points have that value.
    while result.len() < num_points && reader.remaining_bits() >= bpv * 2 {
        let value = reader.read_bits(bpv);
        let count = reader.read_bits(bpv) as usize;
        let count = count.max(1); // ensure at least 1

        let scaled_value = dr.reference_value as f64
            + value as f64
                * 2.0_f64.powi(dr.binary_scale as i32)
                * 10.0_f64.powi(-(dr.decimal_scale as i32));

        let remaining = num_points - result.len();
        let actual_count = count.min(remaining);
        for _ in 0..actual_count {
            result.push(scaled_value);
        }
    }

    // If we haven't filled all points, pad with reference value
    while result.len() < num_points {
        result.push(dr.reference_value as f64);
    }

    Ok(result)
}

/// Template 5.50 / 7.50: Spectral Data — Simple Packing.
///
/// Used by global spectral models. The data contains spectral coefficients
/// packed using simple packing. The first value is the real part of the (0,0)
/// coefficient stored as an IEEE f32, followed by simple-packed remaining coefficients.
fn unpack_spectral_simple(data: &[u8], dr: &DataRepresentation) -> Result<Vec<f64>, String> {
    if data.len() < 4 {
        return Ok(Vec::new());
    }

    // The first 4 bytes are the real value of the (0,0) coefficient as IEEE f32
    let coeff_00 = f32::from_be_bytes([data[0], data[1], data[2], data[3]]) as f64;

    // Remaining data is simple-packed
    let remaining = &data[4..];
    let bpv = dr.bits_per_value as usize;
    if bpv == 0 || remaining.is_empty() {
        return Ok(vec![coeff_00]);
    }

    let total_bits = remaining.len() * 8;
    let n = total_bits / bpv;
    let mut reader = BitReader::new(remaining);
    let mut raw = Vec::with_capacity(n);
    for _ in 0..n {
        raw.push(reader.read_bits(bpv) as i64);
    }

    let mut values = Vec::with_capacity(n + 1);
    values.push(coeff_00);
    values.extend(apply_scaling(&raw, dr));
    Ok(values)
}

/// Template 5.51 / 7.51: Spectral Data — Complex Packing.
///
/// Similar to spectral simple but uses complex packing for the coefficients
/// after the (0,0) term. This is rarely encountered in gridded output.
fn unpack_spectral_complex(data: &[u8], dr: &DataRepresentation) -> Result<Vec<f64>, String> {
    if data.len() < 4 {
        return Ok(Vec::new());
    }

    // The first 4 bytes are the real value of the (0,0) coefficient as IEEE f32
    let coeff_00 = f32::from_be_bytes([data[0], data[1], data[2], data[3]]) as f64;

    // Remaining data uses complex packing
    let remaining = &data[4..];
    if remaining.is_empty() {
        return Ok(vec![coeff_00]);
    }

    let complex_values = unpack_complex(remaining, dr)?;
    let mut values = Vec::with_capacity(complex_values.len() + 1);
    values.push(coeff_00);
    values.extend(complex_values);
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- BitReader tests ----

    #[test]
    fn test_bitreader_read_zero_bits() {
        let data = [0xFF];
        let mut reader = BitReader::new(&data);
        assert_eq!(reader.read_bits(0), 0);
        assert_eq!(reader.remaining_bits(), 8);
    }

    #[test]
    fn test_bitreader_read_single_bit() {
        let data = [0b1010_0000];
        let mut reader = BitReader::new(&data);
        assert_eq!(reader.read_bits(1), 1);
        assert_eq!(reader.read_bits(1), 0);
        assert_eq!(reader.read_bits(1), 1);
        assert_eq!(reader.read_bits(1), 0);
    }

    #[test]
    fn test_bitreader_read_full_byte() {
        let data = [0xA5]; // 1010_0101
        let mut reader = BitReader::new(&data);
        assert_eq!(reader.read_bits(8), 0xA5);
    }

    #[test]
    fn test_bitreader_read_across_byte_boundary() {
        let data = [0xFF, 0x00]; // 11111111 00000000
        let mut reader = BitReader::new(&data);
        assert_eq!(reader.read_bits(4), 0xF); // 1111
        assert_eq!(reader.read_bits(8), 0xF0); // 1111_0000
        assert_eq!(reader.read_bits(4), 0x00); // 0000
    }

    #[test]
    fn test_bitreader_read_16_bits() {
        let data = [0xAB, 0xCD];
        let mut reader = BitReader::new(&data);
        assert_eq!(reader.read_bits(16), 0xABCD);
    }

    #[test]
    fn test_bitreader_read_beyond_data() {
        let data = [0xFF];
        let mut reader = BitReader::new(&data);
        // Reading past the end should pad with zeros
        let val = reader.read_bits(16);
        assert_eq!(val, 0xFF00);
    }

    #[test]
    fn test_bitreader_remaining_bits() {
        let data = [0xFF, 0x00];
        let mut reader = BitReader::new(&data);
        assert_eq!(reader.remaining_bits(), 16);
        reader.read_bits(5);
        assert_eq!(reader.remaining_bits(), 11);
        reader.read_bits(11);
        assert_eq!(reader.remaining_bits(), 0);
    }

    #[test]
    fn test_bitreader_remaining_bits_empty() {
        let data: [u8; 0] = [];
        let reader = BitReader::new(&data);
        assert_eq!(reader.remaining_bits(), 0);
    }

    #[test]
    fn test_bitreader_align_to_byte() {
        let data = [0xFF, 0xAA];
        let mut reader = BitReader::new(&data);
        reader.read_bits(3);
        assert_eq!(reader.remaining_bits(), 13);
        reader.align_to_byte();
        assert_eq!(reader.remaining_bits(), 8);
        assert_eq!(reader.read_bits(8), 0xAA);
    }

    #[test]
    fn test_bitreader_align_already_aligned() {
        let data = [0xFF, 0xAA];
        let mut reader = BitReader::new(&data);
        reader.read_bits(8);
        reader.align_to_byte(); // already aligned, should be no-op
        assert_eq!(reader.remaining_bits(), 8);
    }

    #[test]
    fn test_bitreader_read_signed_zero_bits() {
        let data = [0xFF];
        let mut reader = BitReader::new(&data);
        assert_eq!(reader.read_signed_bits(0), 0);
    }

    #[test]
    fn test_bitreader_read_signed_one_bit() {
        let data = [0xFF];
        let mut reader = BitReader::new(&data);
        // 1 bit: sign-magnitude with just sign bit returns 0
        assert_eq!(reader.read_signed_bits(1), 0);
    }

    #[test]
    fn test_bitreader_read_signed_positive() {
        // 0_0000101 = +5 in 8-bit sign-magnitude
        let data = [0b0_0000101];
        let mut reader = BitReader::new(&data);
        assert_eq!(reader.read_signed_bits(8), 5);
    }

    #[test]
    fn test_bitreader_read_signed_negative() {
        // 1_0000101 = -5 in 8-bit sign-magnitude
        let data = [0b1_0000101];
        let mut reader = BitReader::new(&data);
        assert_eq!(reader.read_signed_bits(8), -5);
    }

    #[test]
    fn test_bitreader_read_signed_negative_large() {
        // 1_1111111 = -127 in 8-bit sign-magnitude
        let data = [0xFF];
        let mut reader = BitReader::new(&data);
        assert_eq!(reader.read_signed_bits(8), -127);
    }

    #[test]
    fn test_bitreader_read_signed_positive_zero() {
        // 0_0000000 = +0
        let data = [0x00];
        let mut reader = BitReader::new(&data);
        assert_eq!(reader.read_signed_bits(8), 0);
    }

    #[test]
    fn test_bitreader_sequential_reads() {
        // Test reading various bit widths sequentially
        let data = [0b11010110, 0b10000000];
        let mut reader = BitReader::new(&data);
        assert_eq!(reader.read_bits(2), 0b11); // 3
        assert_eq!(reader.read_bits(3), 0b010); // 2
        assert_eq!(reader.read_bits(3), 0b110); // 6
        assert_eq!(reader.read_bits(1), 1);
        assert_eq!(reader.remaining_bits(), 7);
    }

    // ---- apply_scaling tests ----

    #[test]
    fn test_apply_scaling_identity() {
        let dr = DataRepresentation {
            template: 0,
            reference_value: 0.0,
            binary_scale: 0,
            decimal_scale: 0,
            bits_per_value: 8,
            ..make_default_dr()
        };
        let raw = vec![1, 2, 3, 4, 5];
        let result = apply_scaling(&raw, &dr);
        for (i, &v) in result.iter().enumerate() {
            assert!((v - (i as f64 + 1.0)).abs() < 1e-10);
        }
    }

    #[test]
    fn test_apply_scaling_with_reference() {
        let dr = DataRepresentation {
            template: 0,
            reference_value: 100.0,
            binary_scale: 0,
            decimal_scale: 0,
            bits_per_value: 8,
            ..make_default_dr()
        };
        let raw = vec![0, 1, 2];
        let result = apply_scaling(&raw, &dr);
        assert!((result[0] - 100.0).abs() < 1e-10);
        assert!((result[1] - 101.0).abs() < 1e-10);
        assert!((result[2] - 102.0).abs() < 1e-10);
    }

    #[test]
    fn test_apply_scaling_with_binary_scale() {
        let dr = DataRepresentation {
            template: 0,
            reference_value: 0.0,
            binary_scale: 1, // multiply by 2
            decimal_scale: 0,
            bits_per_value: 8,
            ..make_default_dr()
        };
        let raw = vec![1, 2, 3];
        let result = apply_scaling(&raw, &dr);
        assert!((result[0] - 2.0).abs() < 1e-10);
        assert!((result[1] - 4.0).abs() < 1e-10);
        assert!((result[2] - 6.0).abs() < 1e-10);
    }

    #[test]
    fn test_apply_scaling_with_decimal_scale() {
        let dr = DataRepresentation {
            template: 0,
            reference_value: 0.0,
            binary_scale: 0,
            decimal_scale: 1, // divide by 10
            bits_per_value: 8,
            ..make_default_dr()
        };
        let raw = vec![10, 20, 30];
        let result = apply_scaling(&raw, &dr);
        assert!((result[0] - 1.0).abs() < 1e-10);
        assert!((result[1] - 2.0).abs() < 1e-10);
        assert!((result[2] - 3.0).abs() < 1e-10);
    }

    // ---- unpack_simple tests ----

    #[test]
    fn test_unpack_simple_zero_bpv() {
        let dr = DataRepresentation {
            template: 0,
            reference_value: 42.0,
            binary_scale: 0,
            decimal_scale: 0,
            bits_per_value: 0,
            ..make_default_dr()
        };
        let data = [0u8; 1];
        let result = unpack_simple(&data, &dr).unwrap();
        assert_eq!(result.len(), 1);
        assert!((result[0] - 42.0).abs() < 1e-10);
    }

    #[test]
    fn test_unpack_simple_8bit() {
        let dr = DataRepresentation {
            template: 0,
            reference_value: 0.0,
            binary_scale: 0,
            decimal_scale: 0,
            bits_per_value: 8,
            ..make_default_dr()
        };
        let data = [10, 20, 30];
        let result = unpack_simple(&data, &dr).unwrap();
        assert_eq!(result.len(), 3);
        assert!((result[0] - 10.0).abs() < 1e-10);
        assert!((result[1] - 20.0).abs() < 1e-10);
        assert!((result[2] - 30.0).abs() < 1e-10);
    }

    // ---- unpack_ieee tests ----

    #[test]
    fn test_unpack_ieee_f32() {
        let dr = DataRepresentation {
            template: 4,
            bits_per_value: 32,
            ..make_default_dr()
        };
        let val: f32 = 3.14;
        let bytes = val.to_be_bytes();
        let result = unpack_ieee(&bytes, &dr).unwrap();
        assert_eq!(result.len(), 1);
        assert!((result[0] - 3.14).abs() < 0.001);
    }

    #[test]
    fn test_unpack_ieee_f64() {
        let dr = DataRepresentation {
            template: 4,
            bits_per_value: 64,
            ..make_default_dr()
        };
        let val: f64 = 2.71828;
        let bytes = val.to_be_bytes();
        let result = unpack_ieee(&bytes, &dr).unwrap();
        assert_eq!(result.len(), 1);
        assert!((result[0] - 2.71828).abs() < 1e-10);
    }

    #[test]
    fn test_unpack_ieee_empty() {
        let dr = DataRepresentation {
            template: 4,
            bits_per_value: 32,
            ..make_default_dr()
        };
        let result = unpack_ieee(&[], &dr).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_unpack_ieee_unsupported_bpv() {
        let dr = DataRepresentation {
            template: 4,
            bits_per_value: 16,
            ..make_default_dr()
        };
        let data = [0u8; 4];
        let result = unpack_ieee(&data, &dr);
        assert!(result.is_err());
    }

    #[test]
    fn test_complex_spatial_primary_and_secondary_missing_skip_recurrence() {
        let dr = DataRepresentation {
            template: 3,
            missing_value_management: 2,
            num_groups: 1,
            group_width_ref: 3,
            last_group_length: 6,
            spatial_diff_order: 2,
            spatial_diff_bytes: 1,
            section5_num_data_points: 6,
            ..make_default_dr()
        };
        let mut data = vec![10, 12, 0];
        data.extend(pack_unsigned(&[0, 0, 7, 1, 6, 2], 3));
        let values = unpack_complex_spatial(&data, &dr).unwrap();
        assert_eq!(values[0], 10.0);
        assert_eq!(values[1], 12.0);
        assert!(values[2].is_nan());
        assert_eq!(values[3], 15.0);
        assert!(values[4].is_nan());
        assert_eq!(values[5], 20.0);
    }

    #[test]
    fn test_complex_width_zero_groups_classify_missing_codes() {
        let dr = DataRepresentation {
            template: 2,
            bits_per_value: 2,
            missing_value_management: 2,
            num_groups: 3,
            group_length_ref: 1,
            last_group_length: 1,
            section5_num_data_points: 3,
            ..make_default_dr()
        };
        let values = unpack_complex(&[0b1110_0100], &dr).unwrap();
        assert!(values[0].is_nan());
        assert!(values[1].is_nan());
        assert_eq!(values[2], 1.0);
    }

    #[test]
    fn test_complex_spatial_recurrence_overflow_is_an_error() {
        let dr = DataRepresentation {
            template: 3,
            num_groups: 1,
            group_width_ref: 1,
            last_group_length: 3,
            spatial_diff_order: 2,
            spatial_diff_bytes: 8,
            section5_num_data_points: 3,
            ..make_default_dr()
        };
        let mut data = Vec::new();
        data.extend_from_slice(&i64::MAX.to_be_bytes());
        data.extend_from_slice(&i64::MAX.to_be_bytes());
        data.extend_from_slice(&0_i64.to_be_bytes());
        data.extend(pack_unsigned(&[0, 0, 1], 1));
        let error = unpack_complex_spatial(&data, &dr).unwrap_err();
        assert!(
            error.contains("second-order spatial recurrence overflow"),
            "{error}"
        );
    }

    fn pack_unsigned(values: &[u64], width: usize) -> Vec<u8> {
        let mut packed = vec![0_u8; (values.len() * width).div_ceil(8)];
        let mut bit = 0usize;
        for value in values {
            for shift in (0..width).rev() {
                if value & (1_u64 << shift) != 0 {
                    packed[bit / 8] |= 1 << (7 - bit % 8);
                }
                bit += 1;
            }
        }
        packed
    }

    // Helper to create a default DataRepresentation for testing
    fn make_default_dr() -> DataRepresentation {
        DataRepresentation {
            template: 0,
            reference_value: 0.0,
            binary_scale: 0,
            decimal_scale: 0,
            bits_per_value: 0,
            original_field_type: 0,
            group_splitting_method: 0,
            missing_value_management: 0,
            primary_missing_value_substitute: 0,
            secondary_missing_value_substitute: 0,
            num_groups: 0,
            group_width_ref: 0,
            group_width_bits: 0,
            group_length_ref: 0,
            group_length_inc: 0,
            group_length_bits: 0,
            last_group_length: 0,
            spatial_diff_order: 0,
            spatial_diff_bytes: 0,
            ccsds_flags: 0,
            ccsds_block_size: 0,
            ccsds_rsi: 0,
            section5_num_data_points: 0,
        }
    }
}
