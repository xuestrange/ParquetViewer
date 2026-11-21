//! Display-only formatting. Source values and exported types remain unchanged.
//!
//! Binary integers and IEEE floats use little-endian Windows byte order. GUIDs
//! use .NET's mixed-endian byte layout; IP addresses use network byte order.

use std::net::{Ipv4Addr, Ipv6Addr};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum BinaryFormat {
    #[default]
    Hex,
    Base64,
    Ascii,
    Size,
    Guid,
    Ipv4,
    Ipv6,
    I16,
    I32,
    I64,
    F32,
    F64,
}

impl BinaryFormat {
    pub fn all() -> &'static [Self] {
        &[
            Self::Hex,
            Self::Base64,
            Self::Ascii,
            Self::Size,
            Self::Guid,
            Self::Ipv4,
            Self::Ipv6,
            Self::I16,
            Self::I32,
            Self::I64,
            Self::F32,
            Self::F64,
        ]
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Hex => "Hex",
            Self::Base64 => "Base64",
            Self::Ascii => "ASCII",
            Self::Size => "Size",
            Self::Guid => "GUID",
            Self::Ipv4 => "IPv4",
            Self::Ipv6 => "IPv6",
            Self::I16 => "Short (16-bit)",
            Self::I32 => "Integer (32-bit)",
            Self::I64 => "Long (64-bit)",
            Self::F32 => "Float (32-bit)",
            Self::F64 => "Double (64-bit)",
        }
    }

    /// Check availability without constructing a formatted value. Only ASCII
    /// inspects content; the other formats need at most the byte width.
    /// Null cells remain the caller's responsibility, as with `apply`.
    pub fn is_supported(self, bytes: &[u8]) -> bool {
        match self {
            Self::Hex | Self::Base64 | Self::Size => true,
            Self::Ascii => {
                !bytes.is_empty()
                    && (bytes.len() < 8
                        || bytes
                            .iter()
                            .filter(|byte| (32..=126).contains(*byte))
                            .count()
                            // ceil(3 * length / 4), without overflowing a
                            // multiplication on unusually large slices.
                            >= bytes.len() - bytes.len() / 4)
            }
            Self::Guid | Self::Ipv6 => bytes.len() == 16,
            Self::Ipv4 | Self::I32 | Self::F32 => bytes.len() == 4,
            Self::I16 => bytes.len() == 2,
            Self::I64 | Self::F64 => bytes.len() == 8,
        }
    }

    /// `None` means the byte width or content does not support this format.
    /// Callers should leave null cells null rather than passing an empty slice.
    pub fn apply(self, bytes: &[u8]) -> Option<String> {
        Some(match self {
            Self::Hex => bytes
                .iter()
                .map(|byte| format!("{byte:02X}"))
                .collect::<Vec<_>>()
                .join("-"),
            Self::Base64 => STANDARD.encode(bytes),
            Self::Size => format!(
                "{} {}",
                bytes.len(),
                if bytes.len() == 1 { "byte" } else { "bytes" }
            ),
            Self::Ascii => {
                // Match the original viewer's ASCII availability heuristic and
                // Encoding.ASCII behavior, including '?' for bytes above 127.
                if bytes.is_empty()
                    || (bytes.len() >= 8
                        && bytes
                            .iter()
                            .filter(|byte| (32..=126).contains(*byte))
                            .count()
                            * 4
                            < bytes.len() * 3)
                {
                    return None;
                }
                bytes
                    .iter()
                    .map(|&byte| if byte < 128 { char::from(byte) } else { '?' })
                    .collect()
            }
            Self::Guid => {
                let value: [u8; 16] = bytes.try_into().ok()?;
                format!(
                    "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
                    u32::from_le_bytes(value[0..4].try_into().ok()?),
                    u16::from_le_bytes(value[4..6].try_into().ok()?),
                    u16::from_le_bytes(value[6..8].try_into().ok()?),
                    value[8],
                    value[9],
                    value[10],
                    value[11],
                    value[12],
                    value[13],
                    value[14],
                    value[15]
                )
            }
            Self::Ipv4 => Ipv4Addr::from(<[u8; 4]>::try_from(bytes).ok()?).to_string(),
            Self::Ipv6 => Ipv6Addr::from(<[u8; 16]>::try_from(bytes).ok()?).to_string(),
            Self::I16 => i16::from_le_bytes(bytes.try_into().ok()?).to_string(),
            Self::I32 => i32::from_le_bytes(bytes.try_into().ok()?).to_string(),
            Self::I64 => i64::from_le_bytes(bytes.try_into().ok()?).to_string(),
            Self::F32 => float_text(f32::from_le_bytes(bytes.try_into().ok()?) as f64, || {
                f32::from_le_bytes(bytes.try_into().unwrap()).to_string()
            }),
            Self::F64 => {
                let value = f64::from_le_bytes(bytes.try_into().ok()?);
                float_text(value, || value.to_string())
            }
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum FloatDisplay {
    #[default]
    Standard,
    Scientific,
    Decimal,
}

impl FloatDisplay {
    pub fn all() -> &'static [Self] {
        &[Self::Standard, Self::Scientific, Self::Decimal]
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Standard => "Standard",
            Self::Scientific => "Scientific",
            Self::Decimal => "Decimal",
        }
    }

    pub fn apply(self, raw: &str, data_type: &str) -> Option<String> {
        if !matches!(
            data_type.to_ascii_uppercase().as_str(),
            "FLOAT" | "REAL" | "DOUBLE"
        ) {
            return None;
        }
        let value = raw.trim().parse::<f64>().ok()?;
        Some(match self {
            Self::Standard => raw.to_owned(),
            Self::Scientific => float_text(value, || format!("{value:e}")),
            Self::Decimal => float_text(value, || expand_decimal(raw.trim())),
        })
    }
}

fn float_text(value: f64, finite: impl FnOnce() -> String) -> String {
    if value.is_nan() {
        "NaN".into()
    } else if value == f64::INFINITY {
        "Infinity".into()
    } else if value == f64::NEG_INFINITY {
        "-Infinity".into()
    } else {
        finite()
    }
}

// Expand the source's shortest decimal representation instead of introducing
// binary rounding artifacts by printing an arbitrary fixed number of places.
fn expand_decimal(raw: &str) -> String {
    let Some((mantissa, exponent)) = raw.split_once(['e', 'E']) else {
        return raw.into();
    };
    let Ok(exponent) = exponent.parse::<i32>() else {
        return raw.into();
    };
    if !(-4096..=4096).contains(&exponent) {
        return raw.into();
    }
    let (sign, mantissa) = if let Some(value) = mantissa.strip_prefix('-') {
        ("-", value)
    } else {
        ("", mantissa.strip_prefix('+').unwrap_or(mantissa))
    };
    let point = mantissa.find('.').unwrap_or(mantissa.len()) as i32 + exponent;
    let digits = mantissa.replace('.', "");
    let value = if point <= 0 {
        format!("0.{}{}", "0".repeat((-point) as usize), digits)
    } else if point as usize >= digits.len() {
        format!("{}{}", digits, "0".repeat(point as usize - digits.len()))
    } else {
        let (left, right) = digits.split_at(point as usize);
        format!("{left}.{right}")
    };
    format!("{sign}{value}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_capabilities_match_conversion_for_all_formats_widths_and_contents() {
        for length in 0..=64 {
            let contents = [
                vec![0; length],
                vec![255; length],
                vec![b'A'; length],
                (0..length).map(|index| index as u8).collect(),
                (0..length)
                    .map(|index| (index.wrapping_mul(131).wrapping_add(31)) as u8)
                    .collect(),
            ];
            for bytes in contents {
                for &format in BinaryFormat::all() {
                    assert_eq!(
                        format.is_supported(&bytes),
                        format.apply(&bytes).is_some(),
                        "{format:?}, bytes={bytes:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn ascii_capability_preserves_short_binary_and_three_quarters_boundary() {
        assert!(!BinaryFormat::Ascii.is_supported(&[]));
        for length in 1..8 {
            assert!(BinaryFormat::Ascii.is_supported(&vec![0xff; length]));
            assert!(BinaryFormat::Ascii.is_supported(&vec![0; length]));
        }
        for length in [8_usize, 9, 10, 11, 12, 15, 16, 31, 32, 33, 257] {
            let printable = (3 * length).div_ceil(4);
            let mut bytes = vec![0xff; length];
            bytes[..printable].fill(b' ');
            assert!(BinaryFormat::Ascii.is_supported(&bytes));
            assert!(BinaryFormat::Ascii.apply(&bytes).is_some());
            bytes[printable - 1] = 127;
            assert!(!BinaryFormat::Ascii.is_supported(&bytes));
            assert!(BinaryFormat::Ascii.apply(&bytes).is_none());
        }
        for format in [BinaryFormat::Hex, BinaryFormat::Base64, BinaryFormat::Size] {
            assert!(format.is_supported(&[]));
        }
    }

    #[test]
    fn binary_layout_matches_windows_and_network_order() {
        assert_eq!(
            BinaryFormat::Hex.apply(&[0, 127, 255]).as_deref(),
            Some("00-7F-FF")
        );
        assert_eq!(
            BinaryFormat::Base64.apply(b"hello").as_deref(),
            Some("aGVsbG8=")
        );
        assert_eq!(
            BinaryFormat::I16.apply(&[0xff, 0xff]).as_deref(),
            Some("-1")
        );
        assert_eq!(BinaryFormat::I32.apply(&[1, 0, 0, 0]).as_deref(), Some("1"));
        assert_eq!(
            BinaryFormat::I64.apply(&i64::MIN.to_le_bytes()).as_deref(),
            Some("-9223372036854775808")
        );
        assert_eq!(
            BinaryFormat::F32.apply(&1.25f32.to_le_bytes()).as_deref(),
            Some("1.25")
        );
        assert_eq!(
            BinaryFormat::F64
                .apply(&f64::INFINITY.to_le_bytes())
                .as_deref(),
            Some("Infinity")
        );
        assert_eq!(
            BinaryFormat::Ipv4.apply(&[192, 168, 0, 1]).as_deref(),
            Some("192.168.0.1")
        );
        assert_eq!(BinaryFormat::Ipv6.apply(&[0; 16]).as_deref(), Some("::"));
        assert_eq!(
            BinaryFormat::Guid
                .apply(&[
                    0x33, 0x22, 0x11, 0x00, 0x55, 0x44, 0x77, 0x66, 0x88, 0x99, 0xaa, 0xbb, 0xcc,
                    0xdd, 0xee, 0xff
                ])
                .as_deref(),
            Some("00112233-4455-6677-8899-aabbccddeeff")
        );
    }

    #[test]
    fn binary_invalid_widths_are_distinct_from_empty_and_null() {
        for format in [
            BinaryFormat::Ipv4,
            BinaryFormat::Ipv6,
            BinaryFormat::Guid,
            BinaryFormat::I16,
            BinaryFormat::I32,
            BinaryFormat::I64,
            BinaryFormat::F32,
            BinaryFormat::F64,
        ] {
            assert_eq!(format.apply(&[]), None);
            assert_eq!(format.apply(&[1, 2, 3]), None);
        }
        assert_eq!(BinaryFormat::Hex.apply(&[]).as_deref(), Some(""));
        assert_eq!(BinaryFormat::Base64.apply(&[]).as_deref(), Some(""));
        assert_eq!(BinaryFormat::Size.apply(&[]).as_deref(), Some("0 bytes"));
        assert_eq!(BinaryFormat::Size.apply(&[0]).as_deref(), Some("1 byte"));
        assert_eq!(BinaryFormat::Ascii.apply(&[]), None);
        assert_eq!(BinaryFormat::Ascii.apply(&[0xff; 8]), None);
        assert_eq!(BinaryFormat::Ascii.apply(&[0xff]).as_deref(), Some("?"));
        assert_eq!(
            BinaryFormat::Ascii.apply(b"abcdef\0\0").as_deref(),
            Some("abcdef\0\0")
        );
    }

    #[test]
    fn decimal_expansion_avoids_rounding_artifacts_and_handles_special_values() {
        assert_eq!(
            FloatDisplay::Decimal
                .apply("1.23456789012345e-20", "DOUBLE")
                .as_deref(),
            Some("0.0000000000000000000123456789012345")
        );
        assert_eq!(
            FloatDisplay::Decimal.apply("-1.25E+6", "FLOAT").as_deref(),
            Some("-1250000")
        );
        assert_eq!(
            FloatDisplay::Scientific.apply("1250", "DOUBLE").as_deref(),
            Some("1.25e3")
        );
        assert_eq!(
            FloatDisplay::Decimal.apply("-0.0", "FLOAT").as_deref(),
            Some("-0.0")
        );
        assert_eq!(
            FloatDisplay::Decimal.apply("NaN", "DOUBLE").as_deref(),
            Some("NaN")
        );
        assert_eq!(
            FloatDisplay::Decimal.apply("-inf", "DOUBLE").as_deref(),
            Some("-Infinity")
        );
        assert_eq!(FloatDisplay::Decimal.apply("NULL", "DOUBLE"), None);
        assert_eq!(FloatDisplay::Scientific.apply("12", "DECIMAL(10,2)"), None);
    }
}
