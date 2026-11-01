//! The logs of what the providers that act through kiln's runners run (docs/ci.md W26-R26): the
//! command each host's runner runs to read them, the same API and stream as a pod's logs.
//!
//! - **Compose** (`compose.kiln.dev/Project`): its services' containers (`docker compose logs`),
//!   one service's or all of them; previous: the newest of its containers that exited, the one a
//!   newer replaced (`docker logs`).
//! - **Host** (`host.kiln.dev/Service`): its process's stdout and stderr. On Linux its user unit's
//!   journal (`journalctl --user`); on macOS the files its launch agent writes them to. A Windows
//!   service's are later.
//!
//! Each takes the last lines (`tail`), from how long ago (`since_seconds`), and whether it follows.
//! Pure: the command is a string for `sh`; the server runs it on the host's runner.

use super::runner::sq;

/// What a person asks of a log.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ask {
    /// The last lines, at the most.
    pub tail: Option<u32>,
    /// From how long ago.
    pub since_seconds: Option<u64>,
    /// Lines as they come, after those.
    pub follow: bool,
    /// The run before this one: a container a newer replaced.
    pub previous: bool,
    /// One of a Compose project's services, or all.
    pub service: Option<String>,
}

/// The lines asked for when none are: as a pod's log is read.
pub const TAIL: u32 = 500;

/// The command that reads object `kind`/`name`'s log on a host running `os` (`linux`, `darwin`,
/// `windows`), or why there's none.
pub fn command(group: &str, kind: &str, name: &str, os: &str, ask: &Ask) -> Result<String, String> {
    let tail = ask.tail.unwrap_or(TAIL).max(1);
    match (group, kind) {
        (super::compose::GROUP, "Project") => Ok(compose(name, tail, ask)),
        (super::host::GROUP, "Service") => host(name, os, tail, ask),
        _ => Err(format!("{group}/{kind} keeps no log kiln reads")),
    }
}

fn compose(project: &str, tail: u32, ask: &Ask) -> String {
    let service = ask.service.as_deref().map(sq);
    if ask.previous {
        let filter = service.as_ref().map(|s| format!(" --filter label=com.docker.compose.service={s}")).unwrap_or_default();
        return format!(
            "id=$(docker ps -a --filter label=com.docker.compose.project={p}{filter} --filter status=exited --format '{{{{.ID}}}}' | head -n 1)\n[ -n \"$id\" ] || {{ echo 'no container of it has exited' >&2; exit 3; }}\ndocker logs --timestamps --tail {tail}{since} \"$id\" 2>&1\n",
            p = sq(project),
            since = since(ask),
        );
    }
    format!(
        "docker compose -p {p} logs --no-color --timestamps --tail {tail}{since}{follow}{service} 2>&1\n",
        p = sq(project),
        since = since(ask),
        follow = if ask.follow { " --follow" } else { "" },
        service = service.map(|s| format!(" {s}")).unwrap_or_default(),
    )
}

fn host(service: &str, os: &str, tail: u32, ask: &Ask) -> Result<String, String> {
    if ask.previous {
        return Err("a host's service keeps one log: its runs follow one another in it".into());
    }
    match os {
        "linux" => Ok(format!(
            "journalctl --user -u {unit} -o short-iso --no-pager -n {tail}{since}{follow} 2>&1\n",
            unit = sq(&format!("kiln-{service}.service")),
            since = ask.since_seconds.map(|s| format!(" --since=-{s}s")).unwrap_or_default(),
            follow = if ask.follow { " --follow" } else { "" },
        )),
        // Its launch agent writes them under the service's directory (`host::agent_log`).
        "darwin" => Ok(format!(
            "D=\"$HOME/.kiln/services/\"{svc}/log\n[ -d \"$D\" ] || {{ echo 'it has written no log yet' >&2; exit 3; }}\ntail -n {tail}{follow} \"$D/out.log\" \"$D/err.log\" 2>&1\n",
            svc = sq(service),
            follow = if ask.follow { " -F" } else { "" },
        )),
        _ => Err(format!("a {os} host's service logs aren't read yet")),
    }
}

/// ` --since Ns`, as Docker takes it.
fn since(ask: &Ask) -> String {
    ask.since_seconds.map(|s| format!(" --since {s}s")).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W26-R26: a Compose project's logs (one service's, previous, following), a Linux host's
    /// journal and a Mac host's files, with tail and since; what keeps none says so.
    #[test]
    fn w26_r26_each_source_reads_its_log_as_asked() {
        let ask = |f: fn(&mut Ask)| {
            let mut a = Ask::default();
            f(&mut a);
            a
        };
        let c = command("compose.kiln.dev", "Project", "shop", "linux", &ask(|a| a.service = Some("web".into()))).unwrap_or_default();
        assert_eq!(c, "docker compose -p 'shop' logs --no-color --timestamps --tail 500 'web' 2>&1\n");
        let c = command("compose.kiln.dev", "Project", "shop", "linux", &ask(|a| {
            a.follow = true;
            a.tail = Some(20);
            a.since_seconds = Some(60);
        }))
        .unwrap_or_default();
        assert_eq!(c, "docker compose -p 'shop' logs --no-color --timestamps --tail 20 --since 60s --follow 2>&1\n");
        let c = command("compose.kiln.dev", "Project", "shop", "linux", &ask(|a| a.previous = true)).unwrap_or_default();
        assert!(c.contains("--filter status=exited") && c.contains("docker logs --timestamps --tail 500 \"$id\""), "{c}");
        let c = command("host.kiln.dev", "Service", "shop-web", "linux", &ask(|a| a.follow = true)).unwrap_or_default();
        assert_eq!(c, "journalctl --user -u 'kiln-shop-web.service' -o short-iso --no-pager -n 500 --follow 2>&1\n");
        let c = command("host.kiln.dev", "Service", "shop-web", "darwin", &Ask::default()).unwrap_or_default();
        assert!(c.contains("tail -n 500 \"$D/out.log\" \"$D/err.log\""), "{c}");
        assert!(command("host.kiln.dev", "Service", "shop-web", "windows", &Ask::default()).is_err());
        assert!(command("host.kiln.dev", "Service", "shop-web", "linux", &ask(|a| a.previous = true)).is_err());
        assert!(command("terraform.kiln.dev", "Stack", "x", "linux", &Ask::default()).is_err());
    }
}
