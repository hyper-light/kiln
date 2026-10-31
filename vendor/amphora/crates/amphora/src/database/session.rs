//! SQL session Records operations reuse the ordinary Records dispatcher on the writer's
//! current transaction. They neither start a transaction nor publish speculative revisions.

use super::*;

fn sql_error(error: anyhow::Error) -> crate::sql::Error {
    crate::sql::Error::Refusal(refused(&error))
}

pub(crate) fn records_get(
    db: &Db,
    namespace: &str,
    family: &str,
    key: &[u8],
) -> crate::sql::Result<Option<Row>> {
    api::family(namespace)?;
    api::family(family)?;
    api::key(key)?;
    let operation = Operation::Get(family.to_owned(), key.to_vec(), Consistency::Linearizable);
    match apply(db, namespace, &operation).map_err(sql_error)?? {
        Answer::Row(row) => Ok(row),
        _ => Err(crate::sql::Error::Refusal(unavailable(
            "wrong session Records get answer",
        ))),
    }
}

pub(crate) fn records_commit(db: &Db, namespace: &str, tx: Tx) -> crate::sql::Result<Rev> {
    api::family(namespace)?;
    api::transaction(&tx)?;
    let operation = Operation::Mutation(Mutation::Records {
        namespace: namespace.to_owned(),
        tx,
    });
    match apply(db, namespace, &operation).map_err(sql_error)?? {
        Answer::Rev(revision) => Ok(revision),
        _ => Err(crate::sql::Error::Refusal(unavailable(
            "wrong session Records commit answer",
        ))),
    }
}
