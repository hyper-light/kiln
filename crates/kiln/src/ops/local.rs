//! U12: local platform commands use the operation descriptor without acquiring a remote
//! route, a Hub, or HTTP error semantics. Their domain handler owns the local evidence.

use serde_json::Value;

/// A local command has no HTTP aliases or remote authorization rule. This marker cannot
/// be registered as an HTTP operation by the server catalog's `Entry` variants.
pub struct Only;

/// U12: a typed empty input for local commands with no arguments.
#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Empty {}

/// The same descriptor and effect flags as server operations, with native local errors.
pub type Op<C = (), T = Value> = super::Op<C, Only, anyhow::Result<T>>;

/// Dispatch a selected local descriptor and record one completion event from its metadata.
/// Local evidence journals should use `operation.name` as their action too. A command with
/// no server Store emits this event; it cannot claim a durable server `audit2` row.
pub async fn invoke<C, T>(operation: &Op<C, T>, context: C, input: Value) -> anyhow::Result<T> {
    let result = match operation.input.fits(&input) {
        Ok(()) => (operation.run)(context, input).await,
        Err(m) => Err(anyhow::anyhow!("{}", m.said(operation.name))),
    };
    tracing::info!(
        target: "kiln::audit",
        action = operation.name,
        transport = "local_cli",
        read_only = operation.read_only,
        destructive = operation.destructive,
        result = if result.is_ok() { "ok" } else { "error" },
        "local operation completed"
    );
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u12_local_operations_share_descriptors_and_stay_out_of_remote_dispatch() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let operation = Op {
                name: "runner.reconcile.inspect",
                summary: "Inspect local migration evidence.",
                read_only: true,
                destructive: false,
                input: super::super::Input::of::<Empty>(),
                route: Only,
                run: |_: (), input: Value| Box::pin(async move {
                    anyhow::ensure!(input.is_object(), "an object is required");
                    Ok(input)
                }),
            };
            let input = serde_json::json!({});
            assert_eq!(invoke(&operation, (), input.clone()).await?, input);
            assert!(invoke(&operation, (), Value::Null).await.is_err());
            assert!(crate::ops::registry().iter().all(|entry| !entry.info().name.starts_with("runner.reconcile.")));
            Ok(())
        })
    }
}
