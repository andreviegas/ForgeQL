//! Parse `BEGIN TRANSACTION`, `COMMIT`, `ROLLBACK`, `VERIFY BUILD`, `DISCONNECT`.
use super::Rule;
use super::helpers::next_str;
use crate::error::ForgeError;
use crate::ir::ForgeQLIR;
pub(super) fn parse_transaction(
    pair: pest::iterators::Pair<'_, Rule>,
) -> Result<ForgeQLIR, ForgeError> {
    let mut inner = pair.into_inner();
    let name = next_str(&mut inner, "transaction: expected name")?;
    Ok(ForgeQLIR::BeginTransaction { name })
}
