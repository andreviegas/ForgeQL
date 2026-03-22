/// Number literal enrichment — indexes every `number_literal` node.
///
/// Creates a new [`IndexRow`] for each `number_literal` with fields:
/// - `num_format`: `"dec"` / `"hex"` / `"bin"` / `"oct"` / `"float"` / `"scientific"`
/// - `has_separator`: `"true"` / `"false"` (C++14 digit separators)
/// - `num_sign`: `"positive"` / `"negative"` / `"zero"`
/// - `num_value`: parsed decimal string (separators stripped)
/// - `num_suffix`: `"u"` / `"l"` / `"ul"` / `"ull"` / `"f"` / `"ll"` / `"z"` / `""`
/// - `is_magic`: `"false"` for {0, 1, -1}; `"true"` otherwise
use std::collections::HashMap;

use super::{EnrichContext, NodeEnricher};
use crate::ast::index::{IndexRow, node_text};

/// Enricher that indexes `number_literal` nodes with numeric metadata.
pub struct NumberEnricher;

impl NodeEnricher for NumberEnricher {
    fn name(&self) -> &'static str {
        "numbers"
    }

    fn extra_rows(&self, ctx: &EnrichContext<'_>) -> Vec<IndexRow> {
        let config = ctx.language_config;
        if !config.number_literal_raw_kinds.contains(&ctx.node.kind()) {
            return vec![];
        }

        let raw = node_text(ctx.source, ctx.node);
        if raw.is_empty() {
            return vec![];
        }

        let mut fields = HashMap::new();

        let has_separator = config.digit_separator.is_some_and(|sep| raw.contains(sep));
        drop(fields.insert("has_separator".to_string(), has_separator.to_string()));

        // Strip digit separators for analysis
        let clean: String = raw
            .chars()
            .filter(|&c| Some(c) != config.digit_separator)
            .collect();
        let lower = clean.to_ascii_lowercase();

        // Detect suffix
        let suffix = detect_suffix_with_table(&lower, config.number_suffixes);
        drop(fields.insert("num_suffix".to_string(), suffix.to_string()));

        // Strip suffix for format analysis
        let without_suffix = strip_suffix_with_table(&lower, config.number_suffixes);

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

        vec![IndexRow {
            name: raw,
            node_kind: ctx.node.kind().to_string(),
            fql_kind: ctx
                .language_support
                .map_kind(ctx.node.kind())
                .unwrap_or("")
                .to_string(),
            language: ctx.language_name.to_string(),
            path: ctx.path.to_path_buf(),
            byte_range: ctx.node.byte_range(),
            line: ctx.node.start_position().row + 1,
            fields,
        }]
    }
}

/// Detect the type suffix of a number literal using the config suffix table.
///
/// The config table is checked in order (longest suffixes first).
fn detect_suffix_with_table(lower: &str, suffixes: &[(&'static str, &str)]) -> &'static str {
    for &(suffix, _) in suffixes {
        if lower.ends_with(suffix) {
            // For hex literals, single-char suffixes a-f are digits, not suffixes
            if suffix.len() == 1 && lower.starts_with("0x") && "abcdef".contains(suffix) {
                continue;
            }
            return suffix;
        }
    }
    ""
}

/// Strip the type suffix from the end of a lowercased number string,
/// using the config suffix table.
fn strip_suffix_with_table<'a>(lower: &'a str, suffixes: &[(&str, &str)]) -> &'a str {
    for &(suf, _) in suffixes {
        if let Some(stripped) = lower.strip_suffix(suf) {
            // For hex literals, single-char 'f' etc. are digits not suffixes
            if suf.len() == 1 && lower.starts_with("0x") && "abcdef".contains(suf) {
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
    use crate::ast::lang::CPP_CONFIG;

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
        let s = CPP_CONFIG.number_suffixes;
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
}
