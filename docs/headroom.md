# Headroom

A machine never gets so busy that what matters on it stops working: on any hardware, without
root, whatever kiln deploys there or runs there.

## Why

On 2026-10-26 adacachy, which runs production kiln on minikube and a runner hosting up to 64 agent
sessions, hung twice. The disks were 16–60% busy: what ran out was the time a sync takes. About
five agents' gates ran `cargo test --workspace` at once, each linking about 100 test binaries of
1.3–2 GB. When Docker tore a container down, the overlay unmount synced the whole host btrfs
(`ovl_sync_fs` → `sync_inodes_sb`) and waited behind those writes for minutes. Docker hung,
production's node went NotReady, and the host locked up. After the reboot, the runner resumed all
47 sessions at once and their gates did it again.

The kernel's own pressure numbers didn't tell this apart from noise either: the same evening,
`/proc/pressure/io` read 60% "full" on an idle machine, because a terminal waiting on io_uring
counts as IO wait.

## How it works

kiln protects outcomes, not resource levels. It measures what matters directly, decides how much
of its own work runs, and only ever slows down what it started itself, which needs no privileges.

**Targets.** What must stay healthy, each with a goal (milliseconds):
- `fsync`: a few bytes written and made durable in a folder. A database commit or a container's
  teardown waits for the same thing on that file system.
- `http`: `GET` an `http://` URL. A 5xx or a refused connection means the target is down for its
  own reasons, not slow, and isn't counted.
- `tcp`: connect to `host:port`.

kiln adds its own targets unless `automatic` is off: an fsync in the runner's data folder (goal
1 s) and the health of the server the runner reports to (goal 2 s). Each target is probed on a
thread of its own every 250 ms. A probe still in flight past its goal counts as a miss while it
hangs, so a stalled sync is seen within the goal, not when it finally returns.

**The window.** How many heavy groups of work may run at full speed, controlled like TCP's
congestion window: while every target meets its goal it grows by one every `grow_secs`; a miss
halves it (at most once per `cooldown_ms`). It starts at `start`, so a restarted runner's sessions
come back a few at a time. Nothing has to be tuned per machine: the window finds each machine's
capacity and follows it.

**Holding.** Before a session sends its next queued message, starts a turn kiln continues after a
restart, or takes a goal, it asks for room. Past the window it waits in kiln's queue and goes out
the moment there's room (the runner wakes held sessions; nothing polls). A message a person sends
"now" never waits. A session let in holds its place for `admit_hold_secs` until its work shows.

**Pacing.** Groups of work past the window are slowed: each runs the first part of every
`period_ms` and is stopped for the rest. That's continuous throttling built from nothing but the
right to pause your own processes. The lowest class and the heaviest writer are slowed first. While
targets miss, paced work runs `min_duty_pct`; after `freeze_after_secs` of misses with the window
at its least, it stops outright. It runs at full speed again only once targets have been healthy
for `release_after_secs`.

A group is heavy when it uses `heavy.cpu_pct` of one CPU or writes `heavy.write_mb_s`. Groups are:
- each agent session's cgroup (`kiln-agents/<session>`);
- with `escapes` on, every transient unit the user's service manager runs (`systemd-run --user`),
  which is how agents start gates that outlive their commands. Units the desktop starts for a
  person (`app-…`, `dbus-…`, `xdg-…`: an app, a terminal window) are never paced.

On Linux, groups are paced with `cgroup.freeze` in the user's own cgroups. Elsewhere there is
nothing to pace yet, and work past the window still waits to start (the status says
`pacing: none`).

**Failing safe.** Everything paced is noted in `<state>/headroom/paced.json`: a runner that dies
with work stopped starts it again when it next starts, and on a clean stop it starts everything
before it goes. Headroom runs on its own threads, from settings it kept, with the server away.
`max_sessions` never turns away sessions a restart continues.

## Settings

Per runner, in the UI (Runners → Headroom) or the API:
- `GET /api/headroom`: every runner that runs headroom.
- `GET /api/runners/:name/headroom`: its settings, whether it applied them, and what it reports.
- `PUT /api/runners/:name/headroom`: new settings (checked, kept, sent to the runner at once).

The server keeps them (`kv` key `headroom/<runner>`) and sends them when the runner connects and
when they change (`ServerMsg::Headroom`, to runners announcing `CAP_HEADROOM`). The runner keeps
them in `<state>/headroom/settings.json` and applies them at start, before any session resumes.
It reports `AgentMsg::HeadroomStatus` when what it decides changes, and latencies at most once a
second.

```json
{
  "enabled": true,
  "automatic": true,
  "targets": [{ "name": "production", "probe": { "kind": "http", "url": "http://192.168.49.2:30070/healthz" }, "goal_ms": 1000 }],
  "window": { "min": 1, "max": 16, "start": 2, "grow_secs": 10, "cooldown_ms": 2000 },
  "pacing": { "enabled": true, "period_ms": 1000, "min_duty_pct": 10, "freeze_after_secs": 20, "release_after_secs": 30 },
  "heavy": { "cpu_pct": 50, "write_mb_s": 5 },
  "admit_hold_secs": 30,
  "escapes": true,
  "max_sessions": null
}
```

## Debug builds

kiln's dev and test builds keep line tables only (`[profile.dev] debug = "line-tables-only"`):
backtraces still give file and line, and each gate writes several times less.

## Next

- Pausing containers, microVMs, VMs and Kubernetes pods through their own controls, and Windows
  and macOS pacing (job objects; SIGSTOP of a tracked tree), with OS limits where available.
- Targets from KilnCD's records of what it deployed on each machine.
- Learned costs per command and leases on the window; merging identical gates; moving work to
  calmer machines; the server pacing what it sends a busy runner, and itself.
- Work on a separate file system where one is writable without root.
