//! `kiln plans`: a plan run's step taken over from the command line, its receipt signed on this
//! device with the key `kiln login` made it (docs/ci.md W27-R08; the Transfers plan's step 6).

use kiln_ci::identity::custody::{Body, Kind, to_value};
use kiln_ci::identity::statement::{PAYLOAD_CUSTODY, Statement, Subject};
use serde_json::{Value, json};

#[derive(clap::Args)]
pub struct PlansArgs {
    #[command(subcommand)]
    cmd: PlansCmd,
}

#[derive(clap::Subcommand)]
enum PlansCmd {
    /// Take step `step` of plan run `run` over: kiln takes it for its agent and hands it to you, as
    /// `kiln login` signed you in, and you sign your receipt of it here, with this device's key.
    Take { run: String, step: usize },
}

pub async fn cli(args: PlansArgs, cfg: &crate::config::Config) -> anyhow::Result<()> {
    match args.cmd {
        PlansCmd::Take { run, step } => take(cfg, &run, step).await,
    }
}

async fn take(cfg: &crate::config::Config, run: &str, step: usize) -> anyhow::Result<()> {
    let taken = crate::identities::post_as_person(cfg, &format!("/api/plans/runs/{run}/steps/{step}/take"), &json!({ "signs": true })).await?;
    let c = taken.get("custody").filter(|c| !c.is_null()).ok_or_else(|| anyhow::anyhow!("kiln took step {step} over but handed you nothing to sign for: it's a kiln from before signed hand-overs"))?;
    let field = |k: &str| c.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    let subject: Subject = serde_json::from_value(c.get("subject").cloned().unwrap_or_default())?;
    let body = Body { from: Some(field("from")), why: format!("step {step} of plan run {run}, taken over"), ..Body::new(Kind::Received, &field("to")) };
    let s = Statement { subject: vec![subject], follows: vec![field("hand_on")], ..Statement::new(to_value(&body)) };
    let receipt = crate::identities::device_signed(PAYLOAD_CUSTODY, &s)?;
    let kept = crate::identities::post_as_person(cfg, &format!("/api/plans/runs/{run}/steps/{step}/receipt"), &json!({ "receipt": receipt })).await?;
    println!("step {step} of plan run {run} is yours: your receipt of it, signed on this device, is {}", kept.get("receipt").and_then(Value::as_str).unwrap_or_default());
    Ok(())
}
