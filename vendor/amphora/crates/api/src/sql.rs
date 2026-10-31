//! U05: a synchronous SQL session on Amphora's writer, for applications whose transaction
//! decisions depend on query results. The owning Database controls commit and durability.
//! Rows and parameters belong to this API; no engine connection escapes to the application.

use std::cell::Cell;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::Value;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Refusal(#[from] crate::Refusal),
    #[error("the query returned no rows")]
    QueryReturnedNoRows,
    #[error("column {0} is outside the result")]
    InvalidColumnIndex(usize),
    #[error("SQL value cannot be read as {0}")]
    InvalidType(&'static str),
    #[error("{0}")]
    Database(String),
}

pub type Result<T> = std::result::Result<T, Error>;

fn failed(e: impl std::fmt::Display) -> Error {
    Error::Database(e.to_string())
}

pub trait OptionalExtension<T> {
    fn optional(self) -> Result<Option<T>>;
}

impl<T> OptionalExtension<T> for Result<T> {
    fn optional(self) -> Result<Option<T>> {
        match self {
            Ok(value) => Ok(Some(value)),
            Err(Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error),
        }
    }
}

impl From<String> for Value {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}
impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Self::Text(value.to_owned())
    }
}
impl From<&String> for Value {
    fn from(value: &String) -> Self {
        Self::Text(value.clone())
    }
}
impl From<Vec<u8>> for Value {
    fn from(value: Vec<u8>) -> Self {
        Self::Blob(value)
    }
}
impl From<&[u8]> for Value {
    fn from(value: &[u8]) -> Self {
        Self::Blob(value.to_vec())
    }
}
impl From<i64> for Value {
    fn from(value: i64) -> Self {
        Self::Integer(value)
    }
}
impl From<i32> for Value {
    fn from(value: i32) -> Self {
        Self::Integer(i64::from(value))
    }
}
impl From<u32> for Value {
    fn from(value: u32) -> Self {
        Self::Integer(i64::from(value))
    }
}
impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Self::Integer(i64::from(value))
    }
}
impl From<f64> for Value {
    fn from(value: f64) -> Self {
        Self::Real(value)
    }
}
impl<T> From<Option<T>> for Value
where
    Value: From<T>,
{
    fn from(value: Option<T>) -> Self {
        value.map_or(Self::Null, Self::from)
    }
}

pub trait ToSql {
    fn to_sql(&self) -> Result<Value>;
}

impl<T: ToSql + ?Sized> ToSql for &T {
    fn to_sql(&self) -> Result<Value> {
        (*self).to_sql()
    }
}
impl ToSql for Value {
    fn to_sql(&self) -> Result<Value> {
        Ok(self.clone())
    }
}
impl ToSql for str {
    fn to_sql(&self) -> Result<Value> {
        Ok(Value::Text(self.to_owned()))
    }
}
impl ToSql for String {
    fn to_sql(&self) -> Result<Value> {
        self.as_str().to_sql()
    }
}
impl ToSql for [u8] {
    fn to_sql(&self) -> Result<Value> {
        Ok(Value::Blob(self.to_vec()))
    }
}
impl ToSql for Vec<u8> {
    fn to_sql(&self) -> Result<Value> {
        self.as_slice().to_sql()
    }
}
impl<const N: usize> ToSql for [u8; N] {
    fn to_sql(&self) -> Result<Value> {
        self.as_slice().to_sql()
    }
}
impl ToSql for bool {
    fn to_sql(&self) -> Result<Value> {
        Ok(Value::Integer(i64::from(*self)))
    }
}
impl ToSql for f64 {
    fn to_sql(&self) -> Result<Value> {
        if self.is_finite() {
            Ok(Value::Real(*self))
        } else {
            Err(Error::InvalidType("a finite real"))
        }
    }
}
impl ToSql for f32 {
    fn to_sql(&self) -> Result<Value> {
        f64::from(*self).to_sql()
    }
}
impl<T: ToSql> ToSql for Option<T> {
    fn to_sql(&self) -> Result<Value> {
        self.as_ref().map_or(Ok(Value::Null), ToSql::to_sql)
    }
}

macro_rules! integer {
    ($($kind:ty),+ $(,)?) => {$(
        impl ToSql for $kind {
            fn to_sql(&self) -> Result<Value> {
                Ok(Value::Integer(i64::try_from(*self).map_err(|_| Error::InvalidType("a signed SQL integer"))?))
            }
        }
        impl FromSql for $kind {
            fn from_sql(value: &Value) -> Result<Self> {
                match value {
                    Value::Integer(n) => Self::try_from(*n).map_err(|_| Error::InvalidType(stringify!($kind))),
                    _ => Err(Error::InvalidType(stringify!($kind))),
                }
            }
        }
    )+};
}
integer!(i8, i16, i32, i64, isize, u8, u16, u32, u64, usize);

/// Conversion failures stay values until the query is submitted; the parameters macro need
/// not return early from an application's closure.
pub struct Parameters(Vec<Result<Value>>);
impl Parameters {
    pub fn new(values: Vec<Result<Value>>) -> Self {
        Self(values)
    }
}

pub trait Params {
    fn values(self) -> Result<Vec<Value>>;
}
impl Params for Parameters {
    fn values(self) -> Result<Vec<Value>> {
        self.0.into_iter().collect()
    }
}
impl Params for [(); 0] {
    fn values(self) -> Result<Vec<Value>> {
        Ok(Vec::new())
    }
}
impl<T: ToSql> Params for Vec<T> {
    fn values(self) -> Result<Vec<Value>> {
        self.iter().map(ToSql::to_sql).collect()
    }
}
impl<T: ToSql> Params for &[T] {
    fn values(self) -> Result<Vec<Value>> {
        self.iter().map(ToSql::to_sql).collect()
    }
}
macro_rules! arrays {
    ($($n:literal),+ $(,)?) => {$(
        impl<T: ToSql> Params for [T; $n] {
            fn values(self) -> Result<Vec<Value>> { self.iter().map(ToSql::to_sql).collect() }
        }
    )+};
}
arrays!(
    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26,
    27, 28, 29, 30, 31, 32
);

#[macro_export]
macro_rules! sql_params {
    ($($value:expr),* $(,)?) => {
        $crate::sql::Parameters::new(vec![$($crate::sql::ToSql::to_sql(&$value)),*])
    };
}
pub use crate::sql_params as params;

pub fn params_from_iter<I: IntoIterator>(values: I) -> Parameters
where
    I::Item: ToSql,
{
    Parameters(values.into_iter().map(|value| value.to_sql()).collect())
}

pub trait FromSql: Sized {
    fn from_sql(value: &Value) -> Result<Self>;
}
impl FromSql for Value {
    fn from_sql(value: &Value) -> Result<Self> {
        Ok(value.clone())
    }
}
impl FromSql for String {
    fn from_sql(value: &Value) -> Result<Self> {
        match value {
            Value::Text(value) => Ok(value.clone()),
            _ => Err(Error::InvalidType("text")),
        }
    }
}
impl FromSql for Vec<u8> {
    fn from_sql(value: &Value) -> Result<Self> {
        match value {
            Value::Blob(value) => Ok(value.clone()),
            _ => Err(Error::InvalidType("blob")),
        }
    }
}
impl FromSql for bool {
    fn from_sql(value: &Value) -> Result<Self> {
        i64::from_sql(value).map(|n| n != 0)
    }
}
impl FromSql for f64 {
    fn from_sql(value: &Value) -> Result<Self> {
        match value {
            Value::Real(value) => Ok(*value),
            Value::Integer(value) => Ok(*value as f64),
            _ => Err(Error::InvalidType("real")),
        }
    }
}
impl<T: FromSql> FromSql for Option<T> {
    fn from_sql(value: &Value) -> Result<Self> {
        match value {
            Value::Null => Ok(None),
            value => T::from_sql(value).map(Some),
        }
    }
}

pub mod types {
    pub use super::{FromSql, ToSql};
    pub use crate::Value;
    #[derive(Debug, Clone, Copy)]
    pub enum ValueRef<'a> {
        Null,
        Integer(i64),
        Real(f64),
        Text(&'a [u8]),
        Blob(&'a [u8]),
    }
}

pub struct Row<'a> {
    values: Vec<Value>,
    lifetime: PhantomData<&'a ()>,
}
impl Row<'_> {
    pub fn column_count(&self) -> usize {
        self.values.len()
    }
    pub fn get<I: TryInto<usize>, T: FromSql>(&self, index: I) -> Result<T> {
        let index = index
            .try_into()
            .map_err(|_| failed("negative or oversized column index"))?;
        T::from_sql(
            self.values
                .get(index)
                .ok_or(Error::InvalidColumnIndex(index))?,
        )
    }
    pub fn get_ref(&self, index: usize) -> Result<types::ValueRef<'_>> {
        Ok(
            match self
                .values
                .get(index)
                .ok_or(Error::InvalidColumnIndex(index))?
            {
                Value::Null => types::ValueRef::Null,
                Value::Integer(n) => types::ValueRef::Integer(*n),
                Value::Real(n) => types::ValueRef::Real(*n),
                Value::Text(value) => types::ValueRef::Text(value.as_bytes()),
                Value::Blob(value) => types::ValueRef::Blob(value),
            },
        )
    }
}

impl<'a> AsRef<Row<'a>> for Row<'a> {
    fn as_ref(&self) -> &Row<'a> {
        self
    }
}

fn size(value: &Value) -> usize {
    match value {
        Value::Text(s) => s.len(),
        Value::Blob(b) => b.len(),
        _ => 8,
    }
}

/// The product supplies this session while it owns the writer transaction. Implementations
/// validate transaction-control boundaries and bound returned bytes before constructing rows.
pub trait Session {
    fn records_get(
        &self,
        _namespace: &str,
        _family: &str,
        _key: &[u8],
    ) -> Result<Option<crate::Row>> {
        Err(crate::invalid("SQL session Records", "unsupported by this session").into())
    }
    fn records_commit(&self, _namespace: &str, _tx: crate::Tx) -> Result<crate::Rev> {
        Err(crate::invalid("SQL session Records", "unsupported by this session").into())
    }
    fn execute_batch(&self, sql: &str) -> Result<()>;
    fn query(&self, sql: &str, params: Vec<Value>, first: bool) -> Result<Vec<Vec<Value>>>;
    fn column_count(&self, sql: &str) -> Result<usize>;
    fn changes(&self) -> i64;
    fn last_insert_rowid(&self) -> i64;
}

pub struct Connection<'db> {
    db: &'db dyn Session,
    poisoned: Cell<bool>,
}
impl<'db> Connection<'db> {
    /// Reads Records from the same transaction as this connection's SQL statements.
    pub fn records_get(
        &self,
        namespace: &str,
        family: &str,
        key: &[u8],
    ) -> Result<Option<crate::Row>> {
        self.finish()?;
        crate::family(namespace)?;
        crate::family(family)?;
        crate::key(key)?;
        self.db.records_get(namespace, family, key)
    }

    /// Applies a Records CAS and writes inside the enclosing Database call. The revision is
    /// provisional until that call succeeds and its durable acknowledgement is delivered.
    pub fn records_commit(&self, namespace: &str, tx: crate::Tx) -> Result<crate::Rev> {
        self.finish()?;
        crate::family(namespace)?;
        crate::transaction(&tx)?;
        self.db.records_commit(namespace, tx)
    }

    pub fn new(db: &'db dyn Session) -> Self {
        Self {
            db,
            poisoned: Cell::new(false),
        }
    }

    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        self.finish()?;
        self.db.execute_batch(sql)
    }

    pub fn execute(&self, sql: &str, params: impl Params) -> Result<usize> {
        self.rows(sql, params, false)?;
        usize::try_from(self.db.changes()).map_err(failed)
    }

    pub fn query_row<T>(
        &self,
        sql: &str,
        params: impl Params,
        mut map: impl FnMut(&Row<'_>) -> Result<T>,
    ) -> Result<T> {
        let rows = self.rows(sql, params, true)?;
        rows.first()
            .ok_or(Error::QueryReturnedNoRows)
            .and_then(&mut map)
    }

    pub fn prepare<'c>(&'c self, sql: &str) -> Result<Statement<'c, 'db>> {
        let columns = self.db.column_count(sql).map_err(failed)?;
        Ok(Statement {
            connection: self,
            sql: sql.to_owned(),
            columns,
        })
    }
    pub fn prepare_cached<'c>(&'c self, sql: &str) -> Result<Statement<'c, 'db>> {
        self.prepare(sql)
    }
    /// The Database checks this before accepting a closure's answer. Failed implicit cleanup
    /// poisons the complete session, which must roll back rather than commit partial work.
    pub fn finish(&self) -> Result<()> {
        if self.poisoned.get() {
            Err(failed(
                "a nested transaction could not roll back; the SQL session must be rolled back",
            ))
        } else {
            Ok(())
        }
    }
    pub fn transaction(&mut self) -> Result<Transaction<'_, 'db>> {
        self.unchecked_transaction()
    }
    pub fn unchecked_transaction(&self) -> Result<Transaction<'_, 'db>> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let id = NEXT
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| failed("SQL savepoint identifiers exhausted"))?;
        let name = format!("amphora_client_{id}");
        self.execute_batch(&format!("SAVEPOINT {name}"))?;
        Ok(Transaction {
            connection: self,
            name,
            done: false,
        })
    }
    pub fn last_insert_rowid(&self) -> i64 {
        self.db.last_insert_rowid()
    }
    pub fn changes(&self) -> u64 {
        u64::try_from(self.db.changes()).unwrap_or(0)
    }

    fn rows(&self, sql: &str, params: impl Params, first: bool) -> Result<Vec<Row<'static>>> {
        self.finish()?;
        let params = params.values()?;
        crate::bound("SQL parameters", params.len(), crate::TX_MAX).map_err(failed)?;
        let mut bytes = sql.len();
        for value in &params {
            bytes = bytes
                .checked_add(size(value))
                .ok_or_else(|| failed("SQL request size overflow"))?;
        }
        crate::bound("SQL request bytes", bytes, crate::RESULT_MAX).map_err(failed)?;
        let rows = self
            .db
            .query(sql, params, first)?
            .into_iter()
            .map(|values| Row {
                values,
                lifetime: PhantomData,
            })
            .collect();
        Ok(rows)
    }
}

pub struct Statement<'c, 'db> {
    connection: &'c Connection<'db>,
    sql: String,
    columns: usize,
}
impl Statement<'_, '_> {
    pub fn exists(&mut self, params: impl Params) -> Result<bool> {
        Ok(!self.connection.rows(&self.sql, params, true)?.is_empty())
    }
    pub fn column_count(&self) -> usize {
        self.columns
    }
    pub fn execute(&mut self, params: impl Params) -> Result<usize> {
        self.connection.execute(&self.sql, params)
    }
    pub fn query_row<T>(
        &mut self,
        params: impl Params,
        map: impl FnMut(&Row<'_>) -> Result<T>,
    ) -> Result<T> {
        self.connection.query_row(&self.sql, params, map)
    }
    pub fn query_map<T>(
        &mut self,
        params: impl Params,
        mut map: impl FnMut(&Row<'_>) -> Result<T>,
    ) -> Result<MappedRows<T>> {
        let rows = self.connection.rows(&self.sql, params, false)?;
        Ok(MappedRows {
            rows: rows.iter().map(&mut map).collect::<Vec<_>>().into_iter(),
        })
    }
    pub fn query(&mut self, params: impl Params) -> Result<Rows> {
        Ok(Rows {
            rows: self.connection.rows(&self.sql, params, false)?.into_iter(),
            current: None,
        })
    }
}
pub struct MappedRows<T> {
    rows: std::vec::IntoIter<Result<T>>,
}
impl<T> Iterator for MappedRows<T> {
    type Item = Result<T>;
    fn next(&mut self) -> Option<Self::Item> {
        self.rows.next()
    }
}
pub struct Rows {
    rows: std::vec::IntoIter<Row<'static>>,
    current: Option<Row<'static>>,
}
impl Rows {
    #[allow(
        clippy::should_implement_trait,
        reason = "This cursor lends its current row; Iterator cannot express that borrowing contract."
    )]
    pub fn next(&mut self) -> Result<Option<&Row<'_>>> {
        self.current = self.rows.next();
        Ok(self.current.as_ref())
    }
}

/// A nested application transaction. Amphora still owns the outer commit and its durability.
pub struct Transaction<'c, 'db> {
    connection: &'c Connection<'db>,
    name: String,
    done: bool,
}
impl<'db> std::ops::Deref for Transaction<'_, 'db> {
    type Target = Connection<'db>;
    fn deref(&self) -> &Self::Target {
        self.connection
    }
}
impl Transaction<'_, '_> {
    pub fn commit(mut self) -> Result<()> {
        self.connection
            .execute_batch(&format!("RELEASE {}", self.name))?;
        self.done = true;
        Ok(())
    }
    pub fn rollback(mut self) -> Result<()> {
        self.connection
            .execute_batch(&format!("ROLLBACK TO {}; RELEASE {}", self.name, self.name))?;
        self.done = true;
        Ok(())
    }
}
impl Drop for Transaction<'_, '_> {
    fn drop(&mut self) {
        if !self.done
            && self
                .connection
                .execute_batch(&format!("ROLLBACK TO {}; RELEASE {}", self.name, self.name))
                .is_err()
        {
            self.connection.poisoned.set(true);
        }
    }
}
