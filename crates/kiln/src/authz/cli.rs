//! `kiln users`, `kiln groups`, `kiln grants`, `kiln roles` and `kiln tokens` (docs/ci.md W10;
//! groups: docs/deploy-api.md §15), over the API.
//! A grant is written `<role or capability>[@<scope>]`, `!` first to deny:
//! `developer@org:acme`, `viewer@repo:acme/app`, `!runs.cancel@repo:acme/app`.

use anyhow::Result;
use serde_json::{Value, json};

use crate::ci::cli::{Api, enc, when};
use crate::config::Config;
use crate::http::client::Method;

#[derive(clap::Args)]
pub struct UsersArgs {
    #[arg(long, env = "KILN_URL", global = true)]
    url: Option<String>,
    /// The admin token, or a personal access token with `users.manage`.
    #[arg(long, env = "KILN_ADMIN_TOKEN", hide_env_values = true, global = true, allow_hyphen_values = true)]
    token: Option<String>,
    #[command(subcommand)]
    cmd: UsersCmd,
}

#[derive(clap::Subcommand)]
enum UsersCmd {
    /// The users and their grants.
    List,
    /// Add a user, with grants to start with; prints a sign-in link to hand them.
    Add {
        name: String,
        #[arg(long = "grant")]
        grants: Vec<String>,
    },
    /// A new one-time sign-in link for a user (the previous one stops working).
    Link { user: String },
    /// End every session of a user.
    SignOut { user: String },
    /// Disable a user: their sessions end and their tokens are revoked.
    Remove { user: String },
}

#[derive(clap::Args)]
pub struct GroupsArgs {
    #[arg(long, env = "KILN_URL", global = true)]
    url: Option<String>,
    /// The admin token, or a personal access token with `users.manage`.
    #[arg(long, env = "KILN_ADMIN_TOKEN", hide_env_values = true, global = true, allow_hyphen_values = true)]
    token: Option<String>,
    #[command(subcommand)]
    cmd: GroupsCmd,
}

#[derive(clap::Subcommand)]
enum GroupsCmd {
    /// kiln's groups and their members.
    List,
    /// A new group, with members to start with (users by name).
    Add {
        name: String,
        #[arg(long = "member")]
        members: Vec<String>,
        #[arg(long, default_value = "")]
        description: String,
    },
    /// Put a user in a group.
    AddMember { group: String, user: String },
    /// Take a user out of a group.
    RemoveMember { group: String, user: String },
    /// Delete a group (refused while it owns apps).
    Remove { name: String },
}

#[derive(clap::Args)]
pub struct GrantsArgs {
    #[arg(long, env = "KILN_URL", global = true)]
    url: Option<String>,
    #[arg(long, env = "KILN_ADMIN_TOKEN", hide_env_values = true, global = true, allow_hyphen_values = true)]
    token: Option<String>,
    #[command(subcommand)]
    cmd: GrantsCmd,
}

#[derive(clap::Subcommand)]
enum GrantsCmd {
    /// Every grant, or a user's.
    List { user: Option<String> },
    /// Grant a user a role or capability in a scope (`developer@org:acme`), or deny it (`!…`).
    Add { user: String, grant: String },
    /// Remove a grant by its id (`kiln grants list`).
    Remove { id: i64 },
}

#[derive(clap::Args)]
pub struct RolesArgs {
    #[arg(long, env = "KILN_URL", global = true)]
    url: Option<String>,
    #[arg(long, env = "KILN_ADMIN_TOKEN", hide_env_values = true, global = true, allow_hyphen_values = true)]
    token: Option<String>,
    #[command(subcommand)]
    cmd: RolesCmd,
}

#[derive(clap::Subcommand)]
enum RolesCmd {
    /// The built-in and custom roles, with their capabilities.
    List,
    /// Define (or redefine) a custom role.
    Set {
        name: String,
        #[arg(required = true)]
        capabilities: Vec<String>,
    },
    /// Remove a custom role.
    Remove { name: String },
}

#[derive(clap::Args)]
pub struct TokensArgs {
    #[arg(long, env = "KILN_URL", global = true)]
    url: Option<String>,
    #[arg(long, env = "KILN_ADMIN_TOKEN", hide_env_values = true, global = true, allow_hyphen_values = true)]
    token: Option<String>,
    #[command(subcommand)]
    cmd: TokensCmd,
}

#[derive(clap::Subcommand)]
enum TokensCmd {
    /// Personal access and agent tokens (yours; everyone's with tokens.manage), never secrets.
    List,
    /// Mint a personal access token (shown once). It does at most what its grants and its user allow.
    Create {
        /// What it may do (repeatable): `developer@repo:acme/app`, `viewer`, `owner` (all its user may).
        #[arg(long = "grant", required = true)]
        grants: Vec<String>,
        /// What it is for.
        #[arg(long)]
        note: Option<String>,
        /// Days it lives (1 to 366).
        #[arg(long, default_value_t = 90)]
        days: i64,
        /// For another user (needs tokens.manage).
        #[arg(long)]
        user: Option<String>,
        /// An agent token for this agent instead: it lives an hour and rotates.
        #[arg(long)]
        agent: Option<String>,
    },
    /// Revoke a token by its id.
    Revoke { id: String },
    /// A new secret for an agent token, good for another hour.
    Rotate { id: String },
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or_default()
}

fn list(v: &Value, k: &str) -> Vec<String> {
    v.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default()
}

/// `kiln users`.
pub async fn users(args: UsersArgs, cfg: &Config) -> Result<()> {
    let api = Api::connect(cfg, args.url.as_deref(), args.token.as_deref()).await?;
    match args.cmd {
        UsersCmd::List => {
            let v = api.call_api(Method::Get, "/api/users", None).await?;
            for u in v.get("users").and_then(Value::as_array).cloned().unwrap_or_default() {
                println!("{:<24} {}  {}", s(&u, "name"), s(&u, "id"), list(&u, "grants").join(" "));
            }
        }
        UsersCmd::Add { name, grants } => {
            let v = api.call_api(Method::Post, "/api/users", Some(json!({ "name": name, "grants": grants }))).await?;
            let link = api.call_api(Method::Post, &format!("/api/users/{}/link", enc(s(&v, "id"))), None).await?;
            println!("added {} ({})\nsign-in link, once, for a day: {}", s(&v, "name"), list(&v, "grants").join(" "), s(&link, "url"));
        }
        UsersCmd::Link { user } => {
            let v = api.call_api(Method::Post, &format!("/api/users/{}/link", enc(&user)), None).await?;
            println!("{}", s(&v, "url"));
        }
        UsersCmd::SignOut { user } => {
            api.call_api(Method::Post, &format!("/api/users/{}/sign-out", enc(&user)), None).await?;
            println!("signed {user} out everywhere");
        }
        UsersCmd::Remove { user } => {
            api.call_api(Method::Delete, &format!("/api/users/{}", enc(&user)), None).await?;
            println!("disabled {user}; their sessions ended and their tokens are revoked");
        }
    }
    Ok(())
}

/// `kiln groups`.
pub async fn groups(args: GroupsArgs, cfg: &Config) -> Result<()> {
    let api = Api::connect(cfg, args.url.as_deref(), args.token.as_deref()).await?;
    match args.cmd {
        GroupsCmd::List => {
            let v = api.call_api(Method::Get, "/api/groups", None).await?;
            for g in v.get("groups").and_then(Value::as_array).cloned().unwrap_or_default() {
                let about = Some(s(&g, "description")).filter(|d| !d.is_empty()).map(|d| format!("  ({d})")).unwrap_or_default();
                println!("{:<24} {}{about}", s(&g, "name"), list(&g, "members").join(" "));
            }
        }
        GroupsCmd::Add { name, members, description } => {
            let v = api.call_api(Method::Post, "/api/groups", Some(json!({ "name": name, "members": members, "description": description }))).await?;
            println!("added {} ({})", s(&v, "name"), list(&v, "members").join(" "));
        }
        GroupsCmd::AddMember { group, user } => {
            api.call_api(Method::Put, &format!("/api/groups/{}/members/{}", enc(&group), enc(&user)), None).await?;
            println!("{user} is in {group}");
        }
        GroupsCmd::RemoveMember { group, user } => {
            api.call_api(Method::Delete, &format!("/api/groups/{}/members/{}", enc(&group), enc(&user)), None).await?;
            println!("{user} is out of {group}");
        }
        GroupsCmd::Remove { name } => {
            api.call_api(Method::Delete, &format!("/api/groups/{}", enc(&name)), None).await?;
            println!("deleted {name}");
        }
    }
    Ok(())
}

/// `kiln grants`.
pub async fn grants(args: GrantsArgs, cfg: &Config) -> Result<()> {
    let api = Api::connect(cfg, args.url.as_deref(), args.token.as_deref()).await?;
    match args.cmd {
        GrantsCmd::List { user } => {
            let q = user.map(|u| format!("?user={}", enc(&u))).unwrap_or_default();
            let v = api.call_api(Method::Get, &format!("/api/grants{q}"), None).await?;
            for g in v.get("grants").and_then(Value::as_array).cloned().unwrap_or_default() {
                let deny = if g.get("deny").and_then(Value::as_bool).unwrap_or(false) { "!" } else { "" };
                println!("{:>5}  {:<20} {deny}{}@{}", g.get("id").and_then(Value::as_i64).unwrap_or_default(), s(&g, "user"), s(&g, "what"), s(&g, "scope"));
            }
        }
        GrantsCmd::Add { user, grant } => {
            let v = api.call_api(Method::Post, "/api/grants", Some(json!({ "user": user, "grant": grant }))).await?;
            println!("{}: {} (grant {})", s(&v, "user"), s(&v, "grant"), v.get("id").and_then(Value::as_i64).unwrap_or_default());
        }
        GrantsCmd::Remove { id } => {
            api.call_api(Method::Delete, &format!("/api/grants/{id}"), None).await?;
            println!("removed grant {id}");
        }
    }
    Ok(())
}

/// `kiln roles`.
pub async fn roles(args: RolesArgs, cfg: &Config) -> Result<()> {
    let api = Api::connect(cfg, args.url.as_deref(), args.token.as_deref()).await?;
    match args.cmd {
        RolesCmd::List => {
            let v = api.call_api(Method::Get, "/api/roles", None).await?;
            for r in v.get("roles").and_then(Value::as_array).cloned().unwrap_or_default() {
                let builtin = if r.get("builtin").and_then(Value::as_bool).unwrap_or(false) { "" } else { " (custom)" };
                println!("{}{builtin}: {}", s(&r, "name"), list(&r, "capabilities").join(" "));
            }
        }
        RolesCmd::Set { name, capabilities } => {
            api.call_api(Method::Put, &format!("/api/roles/{}", enc(&name)), Some(json!({ "capabilities": capabilities }))).await?;
            println!("{name}: {}", capabilities.join(" "));
        }
        RolesCmd::Remove { name } => {
            api.call_api(Method::Delete, &format!("/api/roles/{}", enc(&name)), None).await?;
            println!("removed {name}");
        }
    }
    Ok(())
}

/// `kiln tokens`.
pub async fn tokens(args: TokensArgs, cfg: &Config) -> Result<()> {
    let api = Api::connect(cfg, args.url.as_deref(), args.token.as_deref()).await?;
    match args.cmd {
        TokensCmd::List => {
            let v = api.call_api(Method::Get, "/api/tokens", None).await?;
            for t in v.get("tokens").and_then(Value::as_array).cloned().unwrap_or_default() {
                let state = if t.get("revoked_at").is_some_and(|r| !r.is_null()) { "revoked".to_owned() } else { format!("expires {}", when(t.get("expires_at").and_then(Value::as_i64))) };
                let whose = t.get("on_behalf_of").and_then(Value::as_str).map_or_else(|| s(&t, "principal").to_owned(), |b| format!("{} for {b}", s(&t, "principal")));
                println!("{}  {whose}  {}  {state}, last used {}  {}", s(&t, "masked"), list(&t, "grants").join(" "), when(t.get("last_used_at").and_then(Value::as_i64)), s(&t, "note"));
            }
        }
        TokensCmd::Create { grants, note, days, user, agent } => {
            let kind = if agent.is_some() { "agent" } else { "pat" };
            let body = json!({ "kind": kind, "grants": grants, "note": note, "expires_in_days": days, "user": user, "agent": agent });
            let v = api.call_api(Method::Post, "/api/tokens", Some(body)).await?;
            eprintln!("{} {} ({}), expires {}; shown once:", kind, s(&v, "id"), list(&v, "grants").join(" "), when(v.get("expires_at").and_then(Value::as_i64)));
            println!("{}", s(&v, "token"));
        }
        TokensCmd::Revoke { id } => {
            api.call_api(Method::Delete, &format!("/api/tokens/{}", enc(&id)), None).await?;
            println!("revoked {id}");
        }
        TokensCmd::Rotate { id } => {
            let v = api.call_api(Method::Post, &format!("/api/tokens/{}/rotate", enc(&id)), None).await?;
            eprintln!("rotated {id}, expires {}; shown once:", when(v.get("expires_at").and_then(Value::as_i64)));
            println!("{}", s(&v, "token"));
        }
    }
    Ok(())
}
