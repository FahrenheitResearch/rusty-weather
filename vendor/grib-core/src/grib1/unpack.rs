//! Binary Data Section (BDS) unpacking and IBM float conversion for GRIB1.
//!
//! GRIB1 uses IBM System/360 32-bit floating point format for reference values,
//! which differs from IEEE 754. This module handles the conversion and provides
//! the full unpacking pipeline: extract packed integers, apply binary and decimal
//! scale factors, and produce final floating-point values.

use crate::GribError;

/// Convert an IBM System/360 32-bit float to an IEEE 754 f64.
///
/// IBM format:
/// - Bit 0: sign (0 = positive, 1 = negative)
/// - Bits 1-7: exponent (excess-64, base 16)
/// - Bits 8-31: fraction (24-bit mantissa)
///
/// Value = (-1)^sign * (fraction / 2^24) * 16^(exponent - 64)
///
/// Special case: if the entire word is zero, returns 0.0.
#[inline]
pub fn ibm_to_ieee(ibm: u32) -> f64 {
    if ibm == 0 {
        return 0.0;
    }

    let sign = if ibm & 0x8000_0000 != 0 {
        -1.0_f64
    } else {
        1.0_f64
    };
    let exponent = ((ibm >> 24) & 0x7F) as i32 - 64;
    let mantissa = (ibm & 0x00FF_FFFF) as f64;

    // fraction = mantissa / 2^24, then multiply by 16^exponent
    // 16^exponent = 2^(4*exponent), so total is mantissa * 2^(4*exponent - 24)
    let power = 4 * exponent - 24;
    sign * mantissa * (2.0_f64).powi(power)
}

/// Read a signed 16-bit integer from two bytes (big-endian, sign-magnitude).
///
/// GRIB1 uses sign-magnitude for signed 16-bit values:
/// bit 15 is the sign, bits 14-0 are the magnitude.
#[inline]
fn read_signed_16(hi: u8, lo: u8) -> i16 {
    let magnitude = (((hi & 0x7F) as u16) << 8) | (lo as u16);
    if hi & 0x80 != 0 {
        -(magnitude as i16)
    } else {
        magnitude as i16
    }
}

/// Extract a single datum of `nbits` width from a byte slice starting at bit offset `bit_pos`.
///
/// Returns the extracted unsigned integer value.
#[inline]
fn extract_bits(data: &[u8], bit_pos: usize, nbits: u8) -> u32 {
    if nbits == 0 {
        return 0;
    }

    let mut value: u32 = 0;
    let nbits = nbits as usize;

    for i in 0..nbits {
        let byte_idx = (bit_pos + i) / 8;
        let bit_idx = 7 - ((bit_pos + i) % 8);
        if byte_idx < data.len() {
            value = (value << 1) | (((data[byte_idx] >> bit_idx) & 1) as u32);
        } else {
            value <<= 1;
        }
    }

    value
}

/// Unpack the Binary Data Section (BDS) from raw section bytes.
///
/// # Arguments
/// * `bds_data` - The complete BDS section bytes (starting from byte 1 of the section).
/// * `decimal_scale` - Decimal scale factor D from the PDS (bytes 27-28).
/// * `num_points` - Expected number of data points (from GDS or BMS).
///
/// # Returns
/// A vector of unpacked `f64` values. Missing data (from bitmap) should be handled
/// separately by the caller using the BMS.
///
/// # Unpacking formula
/// ```text
/// Y = R + X * 2^E       (binary scaling)
/// final = Y * 10^(-D)   (decimal scaling)
/// ```
/// where R is the reference value, E is the binary scale factor, X is the packed integer,
/// and D is the decimal scale factor.
pub fn unpack_bds(
    bds_data: &[u8],
    decimal_scale: i16,
    num_points: usize,
) -> Result<Vec<f64>, GribError> {
    if bds_data.len() < 11 {
        return Err(GribError::Unpack(
            "BDS too short: need at least 11 bytes".into(),
        ));
    }

    // Byte 4: flag byte
    let flags = bds_data[3];
    let is_spherical_harmonic = flags & 0x80 != 0;
    let is_complex_packing = flags & 0x40 != 0;
    let _is_integer = flags & 0x20 != 0;
    let _has_additional_flags = flags & 0x10 != 0;

    if is_spherical_harmonic {
        return Err(GribError::Unpack(
            "Spherical harmonic data representation not supported".into(),
        ));
    }
    if is_complex_packing {
        return Err(GribError::Unpack(
            "Complex/second-order packing not supported".into(),
        ));
    }

    // Bytes 5-6: binary scale factor E (signed 16-bit, sign-magnitude)
    let binary_scale = read_signed_16(bds_data[4], bds_data[5]);

    // Bytes 7-10: reference value R (IBM 32-bit float)
    let ibm_ref = ((bds_data[6] as u32) << 24)
        | ((bds_data[7] as u32) << 16)
        | ((bds_data[8] as u32) << 8)
        | (bds_data[9] as u32);
    let reference = ibm_to_ieee(ibm_ref);

    // Byte 11: number of bits per datum
    let nbits = bds_data[10];

    // Compute scaling factors
    let binary_factor = (2.0_f64).powi(binary_scale as i32);
    let decimal_factor = (10.0_f64).powi(-(decimal_scale as i32));

    // Special case: nbits == 0 means all values are the reference value
    if nbits == 0 {
        let value = reference * decimal_factor;
        return Ok(vec![value; num_points]);
    }

    // Packed data starts at byte 12 (index 11)
    let packed_data = &bds_data[11..];
    let mut values = Vec::with_capacity(num_points);

    for i in 0..num_points {
        let bit_pos = i * (nbits as usize);
        let x = extract_bits(packed_data, bit_pos, nbits) as f64;
        let y = reference + x * binary_factor;
        values.push(y * decimal_factor);
    }

    Ok(values)
}

/// Apply a bitmap to unpacked values, inserting `f64::NAN` where the bitmap
/// indicates missing data.
///
/// # Arguments
/// * `values` - The unpacked data values (one per bitmap "1" bit).
/// * `bitmap` - The bitmap bytes from the BMS (excluding the 6-byte header).
/// * `total_points` - The total number of grid points.
///
/// # Returns
/// A vector of length `total_points` with NaN at missing positions.
pub fn apply_bitmap(
    values: &[f64],
    bitmap: &[u8],
    total_points: usize,
) -> Result<Vec<f64>, GribError> {
    let mut result = Vec::with_capacity(total_points);
    let mut val_idx = 0;

    for i in 0..total_points {
        let byte_idx = i / 8;
        let bit_idx = 7 - (i % 8);

        let present = if byte_idx < bitmap.len() {
            (bitmap[byte_idx] >> bit_idx) & 1 == 1
        } else {
            false
        };

        if present {
            if val_idx < values.len() {
                result.push(values[val_idx]);
                val_idx += 1;
            } else {
                result.push(f64::NAN);
            }
        } else {
            result.push(f64::NAN);
        }
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ibm_zero() {
        assert_eq!(ibm_to_ieee(0x0000_0000), 0.0);
    }

    #[test]
    fn test_ibm_one() {
        // IBM representation of 1.0:
        // sign=0, exponent=65 (0x41), mantissa=0x100000
        // value = (0x100000 / 2^24) * 16^(65-64) = (1/16) * 16 = 1.0
        let ibm = 0x4110_0000_u32;
        let ieee = ibm_to_ieee(ibm);
        assert!((ieee - 1.0).abs() < 1e-10, "Expected 1.0, got {}", ieee);
    }

    #[test]
    fn test_ibm_negative() {
        // IBM representation of -1.0:
        // sign=1, exponent=65 (0x41), mantissa=0x100000
        let ibm = 0xC110_0000_u32;
        let ieee = ibm_to_ieee(ibm);
        assert!((ieee - (-1.0)).abs() < 1e-10, "Expected -1.0, got {}", ieee);
    }

    #[test]
    fn test_ibm_small_number() {
        // IBM representation of 0.5:
        // sign=0, exponent=64 (0x40), mantissa=0x800000
        // value = (0x800000 / 2^24) * 16^(64-64) = 0.5 * 1 = 0.5
        let ibm = 0x4080_0000_u32;
        let ieee = ibm_to_ieee(ibm);
        assert!((ieee - 0.5).abs() < 1e-10, "Expected 0.5, got {}", ieee);
    }

    #[test]
    fn test_ibm_large_number() {
        // IBM representation of 100.0:
        // sign=0, exponent=66 (0x42), mantissa=0x640000
        // value = (0x640000 / 2^24) * 16^(66-64) = (100/256) * 256 = 100.0
        let ibm = 0x4264_0000_u32;
        let ieee = ibm_to_ieee(ibm);
        assert!((ieee - 100.0).abs() < 1e-10, "Expected 100.0, got {}", ieee);
    }

    #[test]
    fn test_extract_bits_aligned() {
        let data = [0b1010_1100, 0b0011_0101];
        // First 8 bits: 0b10101100 = 172
        assert_eq!(extract_bits(&data, 0, 8), 172);
        // Next 8 bits: 0b00110101 = 53
        assert_eq!(extract_bits(&data, 8, 8), 53);
    }

    #[test]
    fn test_extract_bits_unaligned() {
        let data = [0b1010_1100, 0b0011_0101];
        // 4 bits starting at bit 4: 1100 = 12
        assert_eq!(extract_bits(&data, 4, 4), 12);
        // 4 bits starting at bit 6: 0000 = 0011 => first 2 from byte 0, next 2 from byte 1
        // bit 6 of byte 0 = 0, bit 7 of byte 0 = 0, bit 0 of byte 1 = 0, bit 1 of byte 1 = 0
        assert_eq!(extract_bits(&data, 6, 4), 0b0000);
    }

    #[test]
    fn test_read_signed_16_positive() {
        // 0x00, 0x0A => +10
        assert_eq!(read_signed_16(0x00, 0x0A), 10);
    }

    #[test]
    fn test_read_signed_16_negative() {
        // 0x80, 0x0A => -10
        assert_eq!(read_signed_16(0x80, 0x0A), -10);
    }

    #[test]
    fn test_unpack_constant_field() {
        // Build a minimal BDS for a constant field (nbits=0).
        // reference = 273.15 in IBM float
        // We use the IBM representation and let the unpacker convert.
        // For simplicity, test with reference=0 (IBM 0x00000000).
        let mut bds = vec![0u8; 11];
        // bytes 1-3: section length = 11
        bds[0] = 0;
        bds[1] = 0;
        bds[2] = 11;
        // byte 4: flags = 0 (grid-point, simple, float, no additional)
        bds[3] = 0;
        // bytes 5-6: binary scale = 0
        bds[4] = 0;
        bds[5] = 0;
        // bytes 7-10: reference value = 0 (IBM zero)
        bds[6] = 0;
        bds[7] = 0;
        bds[8] = 0;
        bds[9] = 0;
        // byte 11: nbits = 0
        bds[10] = 0;

        let values = unpack_bds(&bds, 0, 5).unwrap();
        assert_eq!(values.len(), 5);
        for v in &values {
            assert_eq!(*v, 0.0);
        }
    }

    #[test]
    fn test_apply_bitmap() {
        // bitmap: 1 0 1 1 0 => 3 present values out of 5 total
        let bitmap = vec![0b1011_0000];
        let values = vec![10.0, 20.0, 30.0];
        let result = apply_bitmap(&values, &bitmap, 5).unwrap();
        assert_eq!(result.len(), 5);
        assert_eq!(result[0], 10.0);
        assert!(result[1].is_nan());
        assert_eq!(result[2], 20.0);
        assert_eq!(result[3], 30.0);
        assert!(result[4].is_nan());
    }
}
