//! Parse `FIND SYMBOLS`, `FIND USAGES`, `FIND FILES`, `FIND GLOBALS`, `FIND CALLEES`.
use super::Rule;
use super::clauses::parse_clauses;
use super::helpers::unquote;
use crate::error::ForgeError;
use crate::ir::{CompareOp, ForgeQLIR, Predicate, PredicateValue};
pub(super) fn parse_find(pair: pest::iterators::Pair<'_, Rule>) -> Result<ForgeQLIR, ForgeError> {
    let mut inner = pair.into_inner();
    let target_pair = inner
        .next()
        .ok_or_else(|| ForgeError::DslParse("find: expected target".into()))?;

    // Collect remaining pairs; the only one should be the `clauses` node.
    let clauses = parse_clauses(inner);

    let target_str = target_pair.as_str().trim();

    // "usages OF 'name'" — dedicated variant
    if target_str.starts_with("usages") {
        let name = target_pair
            .into_inner()
            .next()
            .map(|p| unquote(p.as_str()))
            .unwrap_or_default();
        return Ok(ForgeQLIR::FindUsages { of: name, clauses });
    }

    // "callees OF 'func'" — routes to ShowCallees (calls graph query)
    if target_str.starts_with("callees") {
        let symbol = target_pair
            .into_inner()
            .next()
            .map(|p| unquote(p.as_str()))
            .unwrap_or_default();
        return Ok(ForgeQLIR::ShowCallees { symbol, clauses });
    }

    match target_str {
        "globals" => {
            // Convenience alias: FIND globals →
            //   FIND symbols WHERE fql_kind = 'variable' WHERE scope = 'file'
            let kind_pred = Predicate {
                field: "fql_kind".into(),
                op: CompareOp::Eq,
                value: PredicateValue::String("variable".into()),
            };
            let scope_pred = Predicate {
                field: "scope".into(),
                op: CompareOp::Eq,
                value: PredicateValue::String("file".into()),
            };
            let mut clauses = clauses;
            clauses.where_predicates.push(kind_pred);
            clauses.where_predicates.push(scope_pred);
            Ok(ForgeQLIR::FindSymbols { clauses })
        }
        "files" => Ok(ForgeQLIR::FindFiles { clauses }),
        _ => Ok(ForgeQLIR::FindSymbols { clauses }),
    }
}
