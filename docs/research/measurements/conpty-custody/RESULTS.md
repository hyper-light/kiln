# A supervisor as a pseudoconsole's custodian (Windows terminals across an upgrade)

kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 30's follow-up (A3). On macOS a runner that takes
up a new binary hands its PTY masters to `kiln supervise` and the new process takes them over
(`crate::os::supervise_unix`); on Windows every upgrade ends the runner's terminals (docs/releases.md,
"Windows: the supervisor restarts the runner (and its terminals)"), because a pseudoconsole (HPCON) is
no kernel handle `DuplicateHandle` moves. The question: can the supervisor be the pseudoconsole's
custodian from its creation, creating the ConPTY and keeping it and its pipes, its runners only
clients, so an upgrade moves no terminal at all?

**It holds**, with one limit: a pseudoconsole lives exactly as long as its custodian.

## Method

`conpty_custody.rs` (std only, Win32 declared by hand, layouts asserted): one binary in three roles. The
custodian creates the pseudoconsole and its two pipes (`CreatePseudoConsole`), starts the shell in it
(`CreateProcessW` with `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE`), and starts clients, giving each the
input pipe's write end and the output pipe's read end, either duplicated into it (`DuplicateHandle`
with the handle `CreateProcess` returned) or inherited (`PROC_THREAD_ATTRIBUTE_HANDLE_LIST`). It keeps
the HPCON and serves a client's resize (`ResizePseudoConsole`). Scenarios: `probe`, `basic`,
`backlog`, `continuity`, `timing`, `exit`, `shell-exit`, `all`; each prints `RESULT` and `STAT` lines
and keeps them in `<dir>/results.txt`.

Run on jupiter: Windows 11 Pro 25H2 (build 26200.9974), AMD Ryzen AI Max+ 395 (32 threads), Rust 1.99
(msvc), release build, through kiln's runner (session 0, elevated, SeDebugPrivilege on), beside other
builds (21-30 of 32 cores idle). Times by `QueryPerformanceCounter` (10 MHz here). With cmd.exe and
Windows PowerShell 5.1.

```sh
rustc --edition 2024 -O -C debuginfo=1 conpty_custody.rs -o conpty-custody.exe
./conpty-custody.exe all <dir>
```

## Results

- **The shell outlives its client.** Every time: 4 basic runs, 15 with the first client killed
  mid-read, 1,800 hand-overs of one shell. The second client saw a variable the first set (`set`),
  conhost's pid unchanged.
- **What the shell writes while no client reads is held, not lost.** A pipe as `CreatePipe` makes it
  with size 0 (as portable-pty makes it) holds 4096 bytes: it was full 12 ms after its reader
  stopped, holding 4098-4160 bytes, and the shell then blocked (its finish marker didn't appear in
  8 s). The next client got every line once, in order, at 100, 1,000, 10,000 and 100,000 lines
  (7.0 MB). With a 1 MiB pipe, 10,000 lines (677,589 bytes) finished unread in 710 ms, none lost.
- **Exactly once across a hand-over.** 50,000 lines streaming, the first client stopping somewhere
  between line 690 and 2,310, the second taking over 300 ms later: 30 of 30 runs with none missing,
  doubled or out of order, its first line always the first client's last plus one, whether the
  custodian drained the gap (90-227 KB, passed on whole) or not, and with the first client killed
  mid-read. A command the first client typed while the shell was busy ran after the last line and
  before the second client's, 15 of 15.
- **A hand-over's time** (one cmd shell; two runs of n=300, as `a / b`):

  | | p50 | p90 | p99 | max |
  |---|---|---|---|---|
  | warm (next client already running): pipe unread | 0.077 / 0.057 ms | 0.107 / 0.077 | 0.147 / 0.143 | 0.200 / 0.477 |
  | warm: old client stops to new client sees its echo | 0.649 / 0.485 ms | 0.812 / 0.573 | 0.924 / 0.684 | 159.5 / 0.910 |
  | cold (new client started after), `DuplicateHandle`: pipe unread | 21.5 / 18.9 ms | 24.3 / 21.8 | 34.4 / 32.3 | 103.0 / 36.6 |
  | cold, `HANDLE_LIST`: pipe unread | 18.5 / 19.0 ms | 21.1 / 21.2 | 23.4 / 26.1 | 32.7 / 36.0 |

  A cold hand-over is almost all the client's own start (`CreateProcess` to its first line: p50
  17.5-20.3 ms; this small client's, not kiln's, unmeasured). The two warm outliers (159.5 ms, and
  113.6 ms in an earlier n=100 run) were in the shell's echo after the hand-over, the unread gap at
  most 0.2 ms: not root-caused; the shell's echo round trip with one steady client has a p99 near
  16 ms (p50 0.25-0.5 ms) in all three runs. `DuplicateHandle` of a pipe handle: about 0.3 µs
  (n=1000, max 4.4 µs); stopping the old reader (`CancelSynchronousIo` and join): p50 0.09-0.14 ms,
  max 0.77 ms.
- **Resize** through the custodian (the client asks over the custodian's channel, the custodian calls
  `ResizePseudoConsole`): S_OK 4 of 4, the call 9-23 µs, request to answer 0.06-0.13 ms; the shell
  saw 100x40 at once (`mode con`, `[Console]::WindowWidth/Height`).
- **The custodian's end is its terminals' end.** 21 of 21, however it ended (returning without
  `ClosePseudoConsole`, killed, closing first, or `ReleasePseudoConsole` then exiting): the shell
  ended 2.4-4.5 ms later (one at 142 ms) with 0xC000013A, conhost exited, and a client's read broke
  though it held both pipe ends. On 25H2 `ClosePseudoConsole` returns in 8-40 µs, but with the pipe
  full and no reader the shell still ran 5 s later, and ended about 5 ms after the pipe was drained.
- **A shell that exits by itself:** with the custodian keeping the HPCON, conhost stays and the
  client's pipe stays open (3 s later still) until `ClosePseudoConsole` (then broken in about 2 ms);
  after `ReleasePseudoConsole` (S_OK here) the client's pipe broke 2.4-3.1 ms after `exit`, with no
  step of the custodian's, and resizing still worked after it.
- **conhost and the shell are the custodian's children**, in its job and session.
- **kiln's private DACL on a client** (`crate::os::private_memory`) doesn't stop it: the custodian
  reaches the client through its `CreateProcess` handle. `OpenProcess(PROCESS_DUP_HANDLE)` of the
  client is refused once the custodian's SeDebugPrivilege is off: the supervisor pushes handles into
  the runner it started, never the other way.
- **First bytes:** with flags 0, conhost first writes `ESC[?9001h ESC[?1004h`; with portable-pty's
  flags (7) it asks `ESC[6n` first, which whoever reads first must answer.

## What bears on it

- [CreatePseudoConsole](https://learn.microsoft.com/en-us/windows/console/createpseudoconsole): its
  input and output are "restricted to synchronous I/O": a reader needs a thread of its own, stopped
  by `CancelSynchronousIo`; a read that completes as it's cancelled keeps its bytes
  ([CancelSynchronousIo](https://learn.microsoft.com/en-us/windows/win32/fileio/cancelsynchronousio-func)),
  which the old reader passes on before it goes.
- [ClosePseudoConsole](https://learn.microsoft.com/en-us/windows/console/closepseudoconsole): returns
  at once only from Windows 11 24H2 (build 26100); before, it waits until the output is drained.
  [ReleasePseudoConsole](https://learn.microsoft.com/en-us/windows/console/releasepseudoconsole): 24H2
  and later.
- [Creating a Pseudoconsole session](https://learn.microsoft.com/en-us/windows/console/creating-a-pseudoconsole-session):
  relaying the pipes to another process is allowed; closing ends the clients attached and their tree.
- [DuplicateHandle](https://learn.microsoft.com/en-us/windows/win32/api/handleapi/nf-handleapi-duplicatehandle):
  pipes duplicate across processes (the target needs PROCESS_DUP_HANDLE; the value goes over one's
  own channel); an HPCON is no object it lists.
  [Process security and access rights](https://learn.microsoft.com/en-us/windows/win32/procthread/process-security-and-access-rights):
  `CreateProcess`'s handle has PROCESS_ALL_ACCESS; SeDebugPrivilege passes a DACL.
  [UpdateProcThreadAttribute](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-updateprocthreadattribute):
  a `HANDLE_LIST`'s handles must be inheritable, with `bInheritHandles` TRUE.
- microsoft/terminal: `src/winconpty/winconpty.h` (an HPCON is a pointer to three handles in its
  creator's memory: why it can't move), `winconpty.cpp` (conhost started as the caller's child with an
  inherited handle list), `src/host/PtySignalInputThread.cpp` (a broken signal pipe, "the parent
  terminal process has died, or they called ClosePseudoConsole", ends the session: why the
  custodian's end is its terminals').

## What kiln builds on it

portable-pty 0.9.0 (kiln's lock) gives a `MasterPty` with `resize`, `get_size`, `try_clone_reader`
and `take_writer` on Windows, its HPCON and pipe handles private: on Windows kiln drives ConPTY itself
for this (portable-pty stays on Unix). The supervisor (`crate::os::supervise_windows`) becomes the
terminals' custodian: it creates, resizes, lists and closes them, pushes their pipe handles into the
runner it starts, and keeps them across the runner's restarts and upgrades; the runner's terminals
(`crate::terminal`, `crate::os::pty_windows`) ask it over a channel of their own (Windows has none
today: `crate::supervise`'s control socket is Unix's). A long restart either has the supervisor drain
the gap and pass it on, or a larger pipe buys time; with every terminal's pipes in it, the supervisor
may take kiln's private DACL too.
