//! `kiln updates`: the fleet's kiln versions and update policies from the command line, as
//! Settings → Updates shows and sets them (docs/releases.md): `list` (the default), `policy`,
//! `upgrade` and `build`. Talks to a server as `kiln runs` does (`--url`, `--token`, or this
//! machine's own server).

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::ci::cli::Api;
use crate::config::Config;
use crate::http::client::Method;

#[derive(clap::Args)]
pub struct UpdatesArgs {
    #[arg(long, env = "KILN_URL", global = true)]
    url: Option<String>,
    #[arg(long, env = "KILN_ADMIN_TOKEN", hide_env_values = true, global = true, allow_hyphen_values = true)]
    token: Option<String>,
    #[command(subcommand)]
    cmd: Option<UpdatesCmd>,
}

#[derive(clap::Subcommand)]
enum UpdatesCmd {
    /// Every runner: the kiln it runs, its update policy, what's available and where it comes from.
    List,
    /// What runners do with a newer kiln: automatic (upgraded as soon as there's a build),
    /// notify (told, upgraded when you say: the default) or off (left alone). Name the runners,
    /// or say --all.
    Policy {
        /// automatic, notify or off.
        policy: String,
        runners: Vec<String>,
        #[arg(long)]
        all: bool,
    },
    /// Upgrade these runners now (or --all of them). Runners whose updates are off are left alone.
    Upgrade {
        runners: Vec<String>,
        #[arg(long)]
        all: bool,
    },
    /// Build every platform's binary of the commit the server runs.
    Build,
}

pub async fn run(args: UpdatesArgs, cfg: &Config) -> Result<()> {
    let api = Api::connect(cfg, args.url.as_deref(), args.token.as_deref()).await?;
    match args.cmd.unwrap_or(UpdatesCmd::List) {
        UpdatesCmd::List => list(&api).await,
        UpdatesCmd::Policy { policy, runners, all } => {
            let policy = match policy.as_str() {
                "auto" | "automatic" => "auto",
                "notify" => "notify",
                "off" => "off",
                other => bail!("{other:?} isn't a policy: automatic, notify or off"),
            };
            if all == !runners.is_empty() {
                bail!("name the runners, or say --all (not both)");
            }
            let body = if all { json!({ "policy": policy, "all": true }) } else { json!({ "policy": policy, "runners": runners }) };
            api.call_api(Method::Put, "/api/updates/policy", Some(body)).await?;
            println!("updates {} for {}", words(policy), if all { "every runner".to_owned() } else { runners.join(", ") });
            Ok(())
        }
        UpdatesCmd::Upgrade { runners, all } => {
            if all == !runners.is_empty() {
                bail!("name the runners, or say --all (not both)");
            }
            if all {
                let v = api.call_api(Method::Post, "/api/updates/runners", None).await?;
                for j in v.get("jobs").and_then(Value::as_array).into_iter().flatten() {
                    println!("upgrading {}", s(j, "target"));
                }
                for e in v.get("errors").and_then(Value::as_array).into_iter().flatten() {
                    println!("not upgraded: {}", e.as_str().unwrap_or_default());
                }
                return Ok(());
            }
            let mut failed = false;
            for r in &runners {
                match api.call_api(Method::Post, &format!("/api/updates/runners/{}", enc(r)), None).await {
                    Ok(_) => println!("upgrading {r}"),
                    Err(e) => {
                        failed = true;
                        println!("{r}: {e:#}");
                    }
                }
            }
            if failed {
                bail!("not every runner could be upgraded");
            }
            Ok(())
        }
        UpdatesCmd::Build => {
            let v = api.call_api(Method::Post, "/api/updates/build", None).await?;
            println!("building every platform: run #{}", v.get("number").and_then(Value::as_i64).unwrap_or_default());
            Ok(())
        }
    }
}

async fn list(api: &Api) -> Result<()> {
    let v = api.call_api(Method::Get, "/api/updates", None).await?;
    if let Some(n) = v.get("newest").filter(|n| !n.is_null()) {
        println!("newest kiln: {} ({})", short(&s(n, "commit")), s(n, "from"));
    }
    struct Row {
        name: String,
        runs: String,
        policy: String,
        next: String,
    }
    let rows: Vec<Row> = v
        .get("runners")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|r| {
            let runs = r.get("commit").and_then(Value::as_str).map_or_else(|| s(r, "build"), |c| short(c).to_owned());
            let policy = if r.get("locked").and_then(Value::as_bool) == Some(true) { "off (its machine)".to_owned() } else { words(&s(r, "policy")).to_owned() };
            let next = if r.get("connected").and_then(Value::as_bool) != Some(true) {
                "offline".to_owned()
            } else if r.get("up_to_date").and_then(Value::as_bool) == Some(true) {
                "up to date".to_owned()
            } else {
                let (next, from) = (s(r, "next"), s(r, "from"));
                if from.is_empty() { next } else { format!("{next} · {from}") }
            };
            Row { name: s(r, "name"), runs, policy, next }
        })
        .collect();
    let width = |f: fn(&Row) -> &str, head: &str| rows.iter().map(|r| f(r).chars().count()).chain([head.len()]).max().unwrap_or(0);
    let (w0, w1, w2) = (width(|r| &r.name, "RUNNER"), width(|r| &r.runs, "RUNS"), width(|r| &r.policy, "UPDATES"));
    println!("{:<w0$}  {:<w1$}  {:<w2$}  NEXT", "RUNNER", "RUNS", "UPDATES");
    for r in &rows {
        println!("{:<w0$}  {:<w1$}  {:<w2$}  {}", r.name, r.runs, r.policy, r.next);
    }
    Ok(())
}

fn words(policy: &str) -> &str {
    match policy {
        "auto" => "automatic",
        "notify" => "notify",
        "off" => "off",
        other => other,
    }
}

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or_default().to_owned()
}

fn short(c: &str) -> &str {
    c.get(..12).unwrap_or(c)
}

fn enc(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}
