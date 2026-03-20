/// Naming convention and name length enrichment.
///
/// Adds to every named row:
/// - `name_length`: character count of the symbol name
/// - `naming`: detected naming convention
use std::collections::HashMap;

use super::{EnrichContext, NodeEnricher};

/// Enricher that computes `name_length` and `naming` fields.
pub struct NamingEnricher;

impl NodeEnricher for NamingEnricher {
    fn name(&self) -> &'static str {
        "naming"
    }

    fn enrich_row(
        &self,
        _ctx: &EnrichContext<'_>,
        name: &str,
        fields: &mut HashMap<String, String>,
    ) {
        drop(fields.insert("name_length".to_string(), name.len().to_string()));
        drop(fields.insert("naming".to_string(), detect_naming(name).to_string()));
    }
}

/// Detect the naming convention of an identifier.
fn detect_naming(name: &str) -> &'static str {
    let has_underscore = name.contains('_');
    let has_hyphen = name.contains('-');
    let has_upper = name.bytes().any(|b| b.is_ascii_uppercase());
    let has_lower = name.bytes().any(|b| b.is_ascii_lowercase());
    let starts_upper = name.bytes().next().is_some_and(|b| b.is_ascii_uppercase());
    let starts_lower = name.bytes().next().is_some_and(|b| b.is_ascii_lowercase());

    if has_underscore && has_upper && !has_lower {
        "UPPER_SNAKE"
    } else if has_underscore && has_lower {
        "snake_case"
    } else if has_hyphen {
        "kebab-case"
    } else if starts_upper && has_lower {
        "PascalCase"
    } else if starts_lower && has_upper {
        "camelCase"
    } else if has_lower && !has_upper {
        "flatcase"
    } else {
        "mixed"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn naming_conventions() {
        assert_eq!(detect_naming("encenderMotor"), "camelCase");
        assert_eq!(detect_naming("EstadoMotor"), "PascalCase");
        assert_eq!(detect_naming("motor_principal"), "snake_case");
        assert_eq!(detect_naming("VELOCIDAD_MAX"), "UPPER_SNAKE");
        assert_eq!(detect_naming("my-component"), "kebab-case");
        assert_eq!(detect_naming("motorprincipal"), "flatcase");
    }
}
