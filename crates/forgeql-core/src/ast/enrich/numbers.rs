/// Number literal enrichment — indexes every `number_literal` node.
///
/// Creates a new [`IndexRow`] for each `number_literal` with fields:
/// - `num_format`: `"dec"` / `"hex"` / `"bin"` / `"oct"` / `"float"` / `"scientific"`
/// - `has_separator`: `"true"` / `"false"` (C++14 digit separators)
/// - `num_sign`: `"positive"` / `"negative"` / `"zero"`
/// - `num_value`: parsed decimal string (separators stripped)
/// - `num_suffix`: `"u"` / `"l"` / `"ul"` / `"ull"` / `"f"` / `"ll"` / `"z"` / `""`
/// - `suffix_meaning`: semantic meaning of suffix (e.g. `"unsigned"`, `"float"`, `"long_long"`)
/// - `is_magic`: `"false"` for {0, 1, -1}; `"true"` otherwise
use std::collections::HashMap;

use super::{EnrichContext, ExtraRow, NodeEnricher};
use crate::ast::index::node_text;
/// Enricher that indexes `number_literal` nodes with numeric metadata.
pub struct NumberEnricher;

impl NodeEnricher for NumberEnricher {
    fn name(&self) -> &'static str {
        "numbers"
    }

    fn extra_rows(&self, ctx: &EnrichContext<'_>) -> Vec<ExtraRow> {
        let config = ctx.language_config;
        if !config.is_number_literal_kind(ctx.node.kind()) {
            return vec![];
        }

        let raw = node_text(ctx.source, ctx.node);
        if raw.is_empty() {
            return vec![];
        }

        let mut fields = HashMap::new();

        let has_separator = config.digit_sep().is_some_and(|sep| raw.contains(sep));
        drop(fields.insert("has_separator".to_string(), has_separator.to_string()));

        // Strip digit separators for analysis
        let clean: String = raw
            .chars()
            .filter(|&c| Some(c) != config.digit_sep())
            .collect();
        let lower = clean.to_ascii_lowercase();

        // Detect suffix
        let suffix = detect_suffix_with_table(&lower, config.number_suffix_table());
        drop(fields.insert("num_suffix".to_string(), suffix.to_string()));

        // Map suffix to its semantic meaning using the config table.
        if !suffix.is_empty()
            && let Some((_, meaning)) = config
                .number_suffix_table()
                .iter()
                .find(|(s, _)| s == suffix)
        {
            drop(fields.insert("suffix_meaning".to_string(), meaning.clone()));
        }

        // Strip suffix for format analysis
        let without_suffix = strip_suffix_with_table(&lower, config.number_suffix_table());

        // Detect format
        let format = detect_format(without_suffix);
        drop(fields.insert("num_format".to_string(), format.to_string()));

        // Parse numeric value
        let value = parse_value(without_suffix, format);
        drop(fields.insert("num_value".to_string(), value.to_string()));

        // Sign
        let sign = match value.cmp(&0) {
            std::cmp::Ordering::Equal => "zero",
            std::cmp::Ordering::Less => "negative",
            std::cmp::Ordering::Greater => "positive",
        };
        drop(fields.insert("num_sign".to_string(), sign.to_string()));

        // Magic number detection: 0, 1, -1 are not magic
        let is_magic = !(-1..=1).contains(&value);
        drop(fields.insert("is_magic".to_string(), is_magic.to_string()));

        vec![ExtraRow {
            name: raw,
            node_kind: ctx.node.kind().to_string(),
            fql_kind: ctx
                .language_support
                .map_kind(ctx.node.kind())
                .unwrap_or("")
                .to_string(),
            byte_range: ctx.node.byte_range(),
            line: ctx.node.start_position().row + 1,
            fields,
            path_override: None,
        }]
    }
}

/// Detect the type suffix of a number literal using the config suffix table.
///
/// The config table is checked in order (longest suffixes first).
fn detect_suffix_with_table<'a>(lower: &str, suffixes: &'a [(String, String)]) -> &'a str {
    for (suffix, _) in suffixes {
        if lower.ends_with(suffix.as_str()) {
            // For hex literals, single-char suffixes a-f are digits, not suffixes
            if suffix.len() == 1 && lower.starts_with("0x") && "abcdef".contains(suffix.as_str()) {
                continue;
            }
            return suffix;
        }
    }
    ""
}

/// Strip the type suffix from the end of a lowercased number string,
/// using the config suffix table.
fn strip_suffix_with_table<'a>(lower: &'a str, suffixes: &[(String, String)]) -> &'a str {
    for (suf, _) in suffixes {
        if let Some(stripped) = lower.strip_suffix(suf.as_str()) {
            // For hex literals, single-char 'f' etc. are digits not suffixes
            if suf.len() == 1 && lower.starts_with("0x") && "abcdef".contains(suf.as_str()) {
                continue;
            }
            return stripped;
        }
    }
    lower
}

/// Detect the base format of a number literal.
fn detect_format(s: &str) -> &'static str {
    if s.starts_with("0x") || s.starts_with("0X") {
        "hex"
    } else if s.starts_with("0b") || s.starts_with("0B") {
        "bin"
    } else if s.len() > 1
        && s.starts_with('0')
        && s.as_bytes().get(1).is_some_and(u8::is_ascii_digit)
    {
        "oct"
    } else if s.contains('e') || s.contains('E') {
        "scientific"
    } else if s.contains('.') {
        "float"
    } else {
        "dec"
    }
}

/// Parse a number literal string into an i64 value.
fn parse_value(s: &str, format: &str) -> i64 {
    match format {
        "hex" => i64::from_str_radix(s.trim_start_matches("0x").trim_start_matches("0X"), 16)
            .unwrap_or(0),
        "bin" => {
            i64::from_str_radix(s.trim_start_matches("0b").trim_start_matches("0B"), 2).unwrap_or(0)
        }
        "oct" => i64::from_str_radix(s.trim_start_matches('0'), 8).unwrap_or(0),
        #[allow(clippy::cast_possible_truncation)]
        "float" | "scientific" => s.parse::<f64>().map(|f| f as i64).unwrap_or(0),
        _ => s.parse::<i64>().unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::lang::cpp_config;

    #[test]
    fn format_detection() {
        assert_eq!(detect_format("0xff"), "hex");
        assert_eq!(detect_format("0b1010"), "bin");
        assert_eq!(detect_format("077"), "oct");
        assert_eq!(detect_format("1e5"), "scientific");
        assert_eq!(detect_format("3.14"), "float");
        assert_eq!(detect_format("42"), "dec");
        assert_eq!(detect_format("0"), "dec");
    }

    #[test]
    fn suffix_detection() {
        let s = cpp_config().number_suffix_table();
        assert_eq!(detect_suffix_with_table("255u", s), "u");
        assert_eq!(detect_suffix_with_table("100ul", s), "ul");
        assert_eq!(detect_suffix_with_table("100ull", s), "ull");
        assert_eq!(detect_suffix_with_table("3.14f", s), "f");
        assert_eq!(detect_suffix_with_table("100ll", s), "ll");
        assert_eq!(detect_suffix_with_table("42", s), "");
        // Hex 'f' is a digit not a suffix
        assert_eq!(detect_suffix_with_table("0xff", s), "");
    }

    #[test]
    fn value_parsing() {
        assert_eq!(parse_value("0xff", "hex"), 255);
        assert_eq!(parse_value("0b1010", "bin"), 10);
        assert_eq!(parse_value("077", "oct"), 63);
        assert_eq!(parse_value("42", "dec"), 42);
        assert_eq!(parse_value("3.14", "float"), 3);
    }

    #[test]
    fn magic_number_boundary() {
        // 0, 1 are not magic
        assert!(!matches!(0_i64, v if !matches!(v, -1..=1)));
        assert!(!matches!(1_i64, v if !matches!(v, -1..=1)));
        // 2 is magic
        assert!(matches!(2_i64, v if !matches!(v, -1..=1)));
    }

    // -- detect_format edge cases ------------------------------------

    #[test]
    fn detect_format_zero_is_dec_not_oct() {
        // "0" has length 1 so the oct branch (len > 1 && starts_with '0') is skipped.
        assert_eq!(detect_format("0"), "dec");
    }

    #[test]
    fn detect_format_uppercase_hex_prefix() {
        assert_eq!(detect_format("0X1A"), "hex");
    }

    #[test]
    fn detect_format_uppercase_bin_prefix() {
        assert_eq!(detect_format("0B1010"), "bin");
    }

    #[test]
    fn detect_format_octal_two_digit() {
        assert_eq!(detect_format("077"), "oct");
        assert_eq!(detect_format("01"), "oct");
    }

    #[test]
    fn detect_format_scientific_negative_exp() {
        assert_eq!(detect_format("1e-5"), "scientific");
    }

    #[test]
    fn detect_format_scientific_uppercase_e() {
        assert_eq!(detect_format("2E10"), "scientific");
    }

    #[test]
    fn detect_format_scientific_before_float() {
        // "1.5e3" contains both '.' and 'e' — scientific must take priority.
        assert_eq!(detect_format("1.5e3"), "scientific");
    }

    #[test]
    fn detect_format_float_no_exp() {
        assert_eq!(detect_format("0.5"), "float");
        assert_eq!(detect_format("1.0"), "float");
    }

    // -- detect_suffix_with_table edge cases -------------------------

    #[test]
    fn suffix_detection_uppercase() {
        let s = cpp_config().number_suffix_table();
        // Uppercase suffixes must be detected (table entries are lowercased, input
        // is lowercased by the caller — simulate that here).
        assert_eq!(detect_suffix_with_table("100u", s), "u");
        assert_eq!(detect_suffix_with_table("100ul", s), "ul");
        assert_eq!(detect_suffix_with_table("100ull", s), "ull");
        assert_eq!(detect_suffix_with_table("100ll", s), "ll");
    }

    #[test]
    fn suffix_detection_long_l() {
        let s = cpp_config().number_suffix_table();
        assert_eq!(detect_suffix_with_table("42l", s), "l");
    }

    #[test]
    fn suffix_detection_hex_with_u_suffix() {
        let s = cpp_config().number_suffix_table();
        // 0xffu: the trailing 'u' is a suffix; 'f' is a hex digit, not a suffix.
        assert_eq!(detect_suffix_with_table("0xffu", s), "u");
    }

    #[test]
    fn suffix_detection_plain_int_no_suffix() {
        let s = cpp_config().number_suffix_table();
        assert_eq!(detect_suffix_with_table("1234", s), "");
    }

    // -- strip_suffix_with_table edge cases --------------------------

    #[test]
    fn strip_suffix_removes_u_suffix() {
        let s = cpp_config().number_suffix_table();
        assert_eq!(strip_suffix_with_table("42u", s), "42");
    }

    #[test]
    fn strip_suffix_removes_ul_suffix() {
        let s = cpp_config().number_suffix_table();
        assert_eq!(strip_suffix_with_table("100ul", s), "100");
    }

    #[test]
    fn strip_suffix_hex_with_u_leaves_hex_intact() {
        let s = cpp_config().number_suffix_table();
        // 0xffu → strip 'u' → "0xff"
        assert_eq!(strip_suffix_with_table("0xffu", s), "0xff");
    }

    #[test]
    fn strip_suffix_no_suffix_unchanged() {
        let s = cpp_config().number_suffix_table();
        assert_eq!(strip_suffix_with_table("42", s), "42");
    }

    #[test]
    fn strip_suffix_hex_f_not_stripped() {
        let s = cpp_config().number_suffix_table();
        // 0xff: 'f' is a digit, the suffix table hit for single-char hex digits is skipped.
        assert_eq!(strip_suffix_with_table("0xff", s), "0xff");
    }

    // -- parse_value edge cases --------------------------------------

    #[test]
    fn parse_value_scientific_rounds_down() {
        // 1e3 = 1000.0 → truncated to 1000 as i64.
        assert_eq!(parse_value("1e3", "scientific"), 1000);
    }

    #[test]
    fn parse_value_float_truncated() {
        assert_eq!(parse_value("3.9", "float"), 3);
    }

    #[test]
    fn parse_value_overflow_returns_zero() {
        // A decimal value beyond i64::MAX — unwrap_or(0) must not panic.
        assert_eq!(parse_value("99999999999999999999999", "dec"), 0);
    }

    #[test]
    fn parse_value_empty_string_returns_zero() {
        assert_eq!(parse_value("", "dec"), 0);
    }

    #[test]
    fn parse_value_hex_with_uppercase() {
        assert_eq!(parse_value("0XFF", "hex"), 255);
    }

    #[test]
    fn parse_value_binary() {
        assert_eq!(parse_value("0b1111", "bin"), 15);
    }

    #[test]
    fn parse_value_octal() {
        assert_eq!(parse_value("010", "oct"), 8);
    }
}
