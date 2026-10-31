//! U05: Kiln uses the product-owned SQL session API. Legacy SQLite readers import it explicitly.
pub use amphora_api::sql::*;

#[cfg(test)]
pub fn with_memory<R: Send + 'static>(f: impl FnOnce(&mut Connection<'_>) -> anyhow::Result<R> + Send + 'static) -> anyhow::Result<R> {
    let database = amphora::database::Database::in_memory(crate::store::deadline())?;
    database.call(crate::store::deadline(), f).map_err(Into::into)
}
