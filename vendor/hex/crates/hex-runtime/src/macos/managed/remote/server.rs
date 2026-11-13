//! One existing native runtime/table/watcher owner. Requests observe or advance its receipts;
//! the helper never opens a Store, provider, catalog, admission pool or another VMM waiter.
use super::*;
use hex_core::record::{BoxRecord, PoolRecord};

fn own_process() -> Result<ProcId, Refusal> {
    let pid = rustix::process::getpid().as_raw_nonzero().get();
    let actual = sys::rusage(pid)
        .map_err(|error| Refusal::os("reading the helper's own native birth", error))?
        .filter(|actual| !actual.ended && actual.started != 0)
        .ok_or_else(|| super::super::configuration("the helper has no positive native identity"))?;
    Ok(ProcId {
        pid: u32::try_from(pid).map_err(|_| super::super::configuration("helper PID width"))?,
        start_ticks: actual.started,
    })
}

struct PendingReply {
    original: Refusal,
    publication: Option<Refusal>,
    attempts: u64,
}
impl PendingReply {
    fn new(original: Refusal) -> Self {
        Self {
            original,
            publication: None,
            attempts: 0,
        }
    }
    fn failed(&mut self, error: Refusal) {
        // One original failure stays immutable. Publication state retains the latest exact cause;
        // the saturating counter means at least u64::MAX attempts, never another native retry.
        self.attempts = self.attempts.saturating_add(1);
        self.publication = Some(error);
    }
    fn detail(&self) -> Result<protocol::rejected::Rejected, Refusal> {
        let text = match &self.publication {
            None => diagnostic(&self.original)?,
            Some(error) => diagnostic(format_args!(
                "original: {}; publication (attempt {}): {}",
                self.original, self.attempts, error
            ))?,
        };
        Ok(protocol::rejected::Rejected {
            failure: protocol::Failure::Uncertain,
            detail: text.into_bytes(),
        })
    }
    fn send(&self, connection: BorrowedFd<'_>, until: Instant) -> Result<(), Refusal> {
        rustix::net::sockopt::set_socket_nosigpipe(connection, true)
            .map_err(|error| Refusal::os("protecting retained helper rejection", error))?;
        let bytes = protocol::rejected::encode(VERSION, 0, &self.detail()?)
            .map_err(|_| super::super::configuration("encoding retained helper rejection"))?;
        frame::send(
            connection,
            &bytes,
            &[],
            protocol::rejected::MAX_MESSAGE,
            Some(until),
        )
        .map_err(frame::Failure::refusal)
    }
    fn refusal(self) -> Refusal {
        match self.detail() {
            Ok(value) => CallFailure::Rejected(value).refusal(),
            Err(error) => error,
        }
    }
}

pub(super) fn run(
    config: ManagedHelper,
    cpu: protocol::CpuControl,
    rates: hex_core::watch::Rates,
    lateness: Duration,
) -> Result<(), Refusal> {
    let process = own_process()?;
    if std::env::current_exe()
        .map_err(|error| Refusal::os("reading same helper executable", error))?
        != config.binary
    {
        return Err(super::super::configuration(
            "the native helper must be the configured Hex binary",
        ));
    }
    let helper = Helper::new(config, cpu, rates, lateness)?;
    let runtime = MacRuntime::new(rates.bytes, rates.processes, lateness)?.with_cpu_control(cpu)?;
    let mut publisher = receipt::Publisher::new(&helper, process)?;
    let result = run_owned(&helper, &runtime, process, &mut publisher);
    publisher.close_result(result)
}

fn run_owned(
    helper: &Helper,
    runtime: &MacRuntime,
    process: ProcId,
    publisher: &mut receipt::Publisher,
) -> Result<(), Refusal> {
    let startup = (|| {
        rustix::net::sockopt::set_socket_nosigpipe(rustix::stdio::stdout(), true)
            .map_err(|error| Refusal::os("protecting owned helper startup socket", error))?;
        let listener = rustix::net::socket(AddressFamily::UNIX, SocketType::STREAM, None)
            .map_err(|error| Refusal::os("opening native helper listener", error))?;
        super::super::super::private_descriptor(&listener, true)
            .map_err(|error| Refusal::os("opening native helper listener", error))?;
        let address = SocketAddrUnix::new(&helper.config.endpoint)
            .map_err(|error| Refusal::os("naming native helper listener", error))?;
        rustix::net::bind(&listener, &address)
            .map_err(|error| Refusal::os("binding private native helper", error))?;
        rustix::fs::chmod(
            &helper.config.endpoint,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .map_err(|error| Refusal::os("protecting private native helper", error))?;
        publisher.bound(helper)?;
        let ready = protocol::census::encode_answer(
            VERSION,
            0,
            &protocol::census::Census {
                helper: process,
                owners: Vec::new(),
            },
            crate::MAX_BOXES,
        )
        .map_err(|_| super::super::configuration("encoding helper startup readiness"))?;
        frame::send(
            rustix::stdio::stdout(),
            &ready,
            &[],
            protocol::MAX_MESSAGE,
            Some(helper.until(None)?),
        )
        .map_err(frame::Failure::refusal)?;
        Ok(listener)
    })();
    let listener = match startup {
        Ok(listener) => listener,
        Err(error) => {
            let mut failure = PendingReply::new(error);
            if let Err(error) = publisher.finish(
                helper,
                &protocol::completed::Snapshot {
                    helper: process,
                    owners: Vec::new(),
                },
            ) {
                failure.failed(error);
            }
            if let Err(error) = failure.send(rustix::stdio::stdout(), helper.until(None)?) {
                failure.failed(error);
            }
            return Err(failure.refusal());
        }
    };
    let startup = rustix::io::fcntl_dupfd_cloexec(rustix::stdio::stdout(), 3)
        .map_err(|error| Refusal::os("retaining owned first-prepare transport", error))?;
    if let Err(error) = serve(
        helper,
        runtime,
        process,
        &startup,
        publisher,
        Entry::FirstPrepare(&listener),
    ) {
        let mut failure = PendingReply::new(error);
        if let Some(completed) = completed(runtime, process)?
            && let Err(error) = publisher.finish(helper, &completed)
        {
            failure.failed(error);
        }
        if let Err(error) = failure.send(startup.as_fd(), helper.until(None)?) {
            failure.failed(error);
        }
        if publisher.published() {
            return Err(failure.refusal());
        }
        // A submitted first scope stays in the SAME table and canonical Creating record. It
        // must remain reachable for cleanup; neither a broken caller nor an attach error refunds it.
        rustix::net::listen(&listener, libc::SOMAXCONN)
            .map_err(|error| Refusal::os("serving retained first native scope cleanup", error))?;
    }
    drop(startup);
    let null = crate::macos::null(rustix::fs::OFlags::WRONLY)?;
    rustix::stdio::dup2_stdout(&null)
        .map_err(|error| Refusal::os("closing owned helper startup channel", error))?;
    let mut pending: Option<PendingReply> = None;
    loop {
        frame::wait(listener.as_fd(), PollFlags::IN, None).map_err(frame::Failure::refusal)?;
        let connection = match rustix::net::accept(&listener) {
            Ok(connection) => connection,
            Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => continue,
            Err(error) => return Err(Refusal::os("accepting native helper request", error)),
        };
        super::super::super::private_descriptor(&connection, true)
            .map_err(|error| Refusal::os("accepting native helper request", error))?;
        // No blocking Wait is served here. A pending diagnostic uses the next same-UID connection;
        // it owns no native permission, and no dispatch occurs until that error was delivered.
        if let Some(failure) = &mut pending {
            let same =
                sys::peer_euid(&connection).map(|uid| uid == rustix::process::geteuid().as_raw());
            if !matches!(same, Ok(true)) {
                continue;
            }
            match failure.send(connection.as_fd(), helper.until(None)?) {
                Ok(()) => pending = None,
                Err(error) => failure.failed(error),
            }
            continue;
        }
        if let Err(error) = serve(
            helper,
            runtime,
            process,
            &connection,
            publisher,
            Entry::Command,
        ) {
            let mut failure = PendingReply::new(error);
            match failure.send(connection.as_fd(), helper.until(None)?) {
                Ok(()) => {}
                Err(error) => {
                    failure.failed(error);
                    pending = Some(failure);
                }
            }
        }
        if publisher.published() {
            // All VMM receipts were finalized after their durable boot release before this
            // completion snapshot. The node still retains its final canonical Draining charge
            // until its existing helper observer proves/reaps this process's actual termination.
            return match pending {
                Some(failure) => Err(failure.refusal()),
                None => Ok(()),
            };
        }
    }
}

enum Entry<'a> {
    FirstPrepare(&'a OwnedFd),
    Command,
}

fn serve(
    helper: &Helper,
    runtime: &MacRuntime,
    process: ProcId,
    connection: &OwnedFd,
    publisher: &mut receipt::Publisher,
    entry: Entry<'_>,
) -> Result<(), Refusal> {
    if sys::peer_euid(connection)
        .map_err(|error| Refusal::os("reading native client UID", error))?
        != rustix::process::geteuid().as_raw()
    {
        return Err(super::super::configuration(
            "native helper request belongs to another user",
        ));
    }
    // A command client gone before its request was read asked nothing, and no effect was made: no
    // reply is owed, to it or to the next client. The first Prepare keeps its startup failure.
    let command = matches!(entry, Entry::Command);
    let pid = match sys::peer_pid(connection) {
        Ok(pid) => pid,
        Err(sys::PeerPidFailure::Native(error))
            if command && error.raw_os_error() == Some(libc::ENOTCONN) =>
        {
            return Ok(());
        }
        Err(failure) => {
            return Err(Refusal::Os {
                op: "reading the native client PID",
                why: diagnostic(failure)?,
            });
        }
    };
    let birth = sys::rusage(pid)
        .map_err(|error| Refusal::os("reading native client birth", error))?
        .filter(|actual| !actual.ended && actual.started != 0)
        .ok_or_else(|| super::super::configuration("native client is no longer live"))?;
    let peer_id = ProcId {
        pid: u32::try_from(pid)
            .map_err(|_| super::super::configuration("native client PID width"))?,
        start_ticks: birth.started,
    };
    rustix::net::sockopt::set_socket_nosigpipe(connection, true)
        .map_err(|error| Refusal::os("protecting helper connection disconnect", error))?;
    let until = helper.until(None)?;
    let (bytes, descriptors) = match frame::receive(
        connection.as_fd(),
        protocol::timed::MAX_MESSAGE,
        Some(until),
    ) {
        Ok(received) => received,
        Err(frame::Failure::Closed) if command => return Ok(()),
        Err(failure) => return Err(failure.refusal()),
    };
    peer(connection, peer_id)?;
    if let Ok((version, id, wanted)) = protocol::census::read_request(&bytes, descriptors.len()) {
        if matches!(entry, Entry::FirstPrepare(_)) {
            return Err(super::super::configuration(
                "first helper request must be scoped Prepare",
            ));
        }
        if version != VERSION || wanted != process {
            return Err(super::super::configuration("foreign native census helper"));
        }
        let snapshot = census(runtime, process)?;
        let answer = protocol::census::encode_answer(VERSION, id, &snapshot, crate::MAX_BOXES)
            .map_err(|_| super::super::configuration("encoding native owner census"))?;
        return frame::send(
            connection.as_fd(),
            &answer,
            &[],
            protocol::census::message_bound(crate::MAX_BOXES)
                .map_err(|_| native_allocation(crate::MAX_BOXES))?,
            Some(until),
        )
        .map_err(frame::Failure::refusal);
    }
    // Joint plan step 14 (C4 on macOS): the node's learner sets a scope's CPU rate to its water-filled share.
    if let Ok((version, id, key, rate)) = protocol::rate::read(&bytes, descriptors.len()) {
        if matches!(entry, Entry::FirstPrepare(_)) {
            return Err(super::super::configuration(
                "first helper request must be scoped Prepare",
            ));
        }
        if version != VERSION || key.helper != process {
            return Err(super::super::configuration(
                "foreign native helper incarnation",
            ));
        }
        let answer =
            match get(runtime, &key, None).and_then(|boxed| boxed.set_managed_rate(rate)) {
                Ok(()) => protocol::encode_answer(VERSION, id, &protocol::Answer::Done { key }),
                Err(error) => {
                    protocol::rejected::encode(VERSION, id, &PendingReply::new(error).detail()?)
                }
            }
            .map_err(|_| super::super::configuration("encoding a native rate answer"))?;
        return frame::send(
            connection.as_fd(),
            &answer,
            &[],
            protocol::MAX_MESSAGE.max(protocol::rejected::MAX_MESSAGE),
            Some(until),
        )
        .map_err(frame::Failure::refusal);
    }
    // Joint plan step 14 (2c; ROOT, 2026-11-13): the node asks the slowest stop response this helper's managed CPU
    // control has seen in its own holds, which it derives its charge window from: none before the first.
    if let Ok((version, id)) = protocol::response::read(&bytes, descriptors.len()) {
        if matches!(entry, Entry::FirstPrepare(_)) {
            return Err(super::super::configuration(
                "first helper request must be scoped Prepare",
            ));
        }
        if version != VERSION {
            return Err(super::super::configuration(
                "foreign native helper incarnation",
            ));
        }
        let answer = protocol::response::encode_answer(VERSION, id, runtime.cpu_response()?)
            .map_err(|_| super::super::configuration("encoding a native response answer"))?;
        return frame::send(
            connection.as_fd(),
            &answer,
            &[],
            protocol::response::MAX_ANSWER,
            Some(until),
        )
        .map_err(frame::Failure::refusal);
    }
    let (version, id, expires, request) = protocol::timed::read(&bytes, descriptors.len())
        .map_err(|_| super::super::configuration("invalid bounded native request"))?;
    let until = deadline(expires, helper.config.rpc_budget)?.min(until);
    if matches!(entry, Entry::FirstPrepare(_))
        && !matches!(&request, protocol::Request::Prepare { .. })
    {
        return Err(super::super::configuration(
            "first helper request must be scoped Prepare",
        ));
    }
    if version != VERSION || request.key().helper != process {
        return Err(super::super::configuration(
            "foreign native helper incarnation",
        ));
    }
    if publisher.pending() && matches!(request, protocol::Request::Prepare { .. }) {
        return Err(super::super::configuration(
            "native helper completion publication is pending",
        ));
    }
    let (answer, notification) = match dispatch(helper, runtime, &request, descriptors, until) {
        Ok(result) => result,
        Err(error) => {
            if matches!(entry, Entry::FirstPrepare(_)) {
                return Err(error);
            }
            let mut failure = PendingReply::new(error);
            let bytes = protocol::rejected::encode(VERSION, id, &failure.detail()?)
                .map_err(|_| super::super::configuration("encoding native request rejection"))?;
            return match frame::send(
                connection.as_fd(),
                &bytes,
                &[],
                protocol::rejected::MAX_MESSAGE,
                Some(until),
            ) {
                Ok(()) => Ok(()),
                Err(error) => {
                    failure.failed(error.refusal());
                    Err(failure.refusal())
                }
            };
        }
    };
    if matches!(
        &request,
        protocol::Request::Control {
            action: protocol::Action::Finalize,
            ..
        }
    ) && matches!(&answer, protocol::Answer::Done { .. })
        && let Some(completed) = completed(runtime, process)?
    {
        publisher.finish(helper, &completed)?;
    }
    if let Entry::FirstPrepare(listener) = entry {
        // The first scope is already retained; public service is ready before Attached is sent.
        rustix::net::listen(listener, libc::SOMAXCONN)
            .map_err(|error| Refusal::os("activating scoped native helper service", error))?;
    }
    let bytes = protocol::encode_answer(VERSION, id, &answer)
        .map_err(|_| super::super::configuration("encoding native helper answer"))?;
    let descriptors = notification.as_ref().map(AsFd::as_fd);
    let borrowed = match &descriptors {
        Some(fd) => std::slice::from_ref(fd),
        None => &[],
    };
    // SCM_RIGHTS sender retains the same notification object through the successful send. Its
    // other writer remains in the native Scope; no ownership is inferred from socket names.
    frame::send(
        connection.as_fd(),
        &bytes,
        borrowed,
        protocol::MAX_MESSAGE,
        Some(until),
    )
    .map_err(frame::Failure::refusal)
}

/// Each record kept boxed: the two differ in size by more than the workspace's variant bound.
enum Record {
    Box(Box<BoxRecord>),
    Pool(Box<PoolRecord>),
}
impl Record {
    fn decode(launch: &protocol::Launch) -> Result<Self, Refusal> {
        let record = match launch.record_kind {
            protocol::RecordKind::Box => {
                Self::Box(Box::new(serde_json::from_slice(&launch.record).map_err(
                    |_| super::super::configuration("invalid canonical native Box record"),
                )?))
            }
            protocol::RecordKind::Pool => Self::Pool(Box::new(PoolRecord::decode(&launch.record)?)),
        };
        match record.as_ref() {
            NativeRecord::Box(record) => record.spec.check()?,
            NativeRecord::Pool(record) => record.check()?,
        };
        Ok(record)
    }
    fn as_ref(&self) -> NativeRecord<'_> {
        match self {
            Self::Box(record) => NativeRecord::Box(record),
            Self::Pool(record) => NativeRecord::Pool(record),
        }
    }
}

fn get(
    runtime: &MacRuntime,
    key: &protocol::Key,
    controller: Option<ProcId>,
) -> Result<Arc<ProcessBox>, Refusal> {
    let boxed = {
        let boxes = runtime.boxes()?;
        let at = crate::macos::native_owner_index(&boxes, key.owner.as_ref())
            .map_err(|_| super::super::configuration("unknown native helper owner"))?;
        Arc::clone(
            &boxes
                .get(at)
                .ok_or_else(|| native_allocation(boxes.len()))?
                .1,
        )
    };
    let scope = boxed
        .scope()
        .ok_or_else(|| super::super::configuration("native helper slot has no scope"))?;
    if scope.scope != key.scope
        || scope.commitment != key.commitment
        || controller.is_some_and(|controller| {
            boxed.identity.get().is_none_or(|identity| {
                identity.pid != controller.pid || identity.started != controller.start_ticks
            })
        })
    {
        return Err(scope.refusal(boxed.owner()));
    }
    Ok(boxed)
}

fn attached(
    boxed: &Arc<ProcessBox>,
    key: protocol::Key,
) -> Result<(protocol::Answer, Option<OwnedFd>), Refusal> {
    let (reader, writer) = std::io::pipe()
        .map(|(reader, writer)| (OwnedFd::from(reader), OwnedFd::from(writer)))
        .map_err(|error| Refusal::os("preparing a private native scope notification", error))?;
    for end in [&reader, &writer] {
        super::super::super::private_descriptor(end, true)
            .map_err(|error| Refusal::os("preparing a private native scope notification", error))?;
    }
    let scope = boxed
        .scope()
        .ok_or_else(|| super::super::configuration("native helper slot has no scope"))?;
    *scope
        .notification
        .lock()
        .map_err(|_| scope.refusal(boxed.owner()))? = Some(writer);
    // Covers a controller/member that became terminal before the new pipe was attached. This is
    // a notification only; every proxy query still proves the exact same native scope.
    scope.notify().map_err(|_| scope.refusal(boxed.owner()))?;
    let controller = boxed.identity.get().map(|identity| ProcId {
        pid: identity.pid,
        start_ticks: identity.started,
    });
    let vsock = scope
        .vsock
        .get()
        .map(|vsock| copy_raw(vsock.as_os_str().as_bytes()))
        .transpose()?
        .ok_or_else(|| scope.refusal(boxed.owner()))?;
    Ok((
        protocol::Answer::Attached {
            key,
            controller,
            vsock,
        },
        Some(reader),
    ))
}

fn dispatch(
    helper: &Helper,
    runtime: &MacRuntime,
    request: &protocol::Request,
    descriptors: Vec<OwnedFd>,
    until: Instant,
) -> Result<(protocol::Answer, Option<OwnedFd>), Refusal> {
    if until <= Instant::now() {
        return Err(super::super::configuration(
            "native helper request expired before effects",
        ));
    }
    let key = request.key();
    if key.helper != own_process()? {
        return Err(super::super::configuration(
            "native helper incarnation mismatch",
        ));
    }
    match request {
        protocol::Request::Prepare { launch, .. } => {
            launch
                .check()
                .map_err(|_| super::super::configuration("invalid native prepared launch"))?;
            if launch.cpu != helper.cpu {
                return Err(super::super::configuration(
                    "native launch CPU profile differs from initialized helper",
                ));
            }
            let owned = Record::decode(launch)?;
            let record = owned.as_ref();
            if !key.matches(record, key.helper)?
                || record.stage() != hex_core::record::NativeStage::Creating
                || record.init().is_some()
            {
                return Err(record.refusal(LifecycleCause::Uncertain));
            }
            if record.devices().is_empty() != launch.report_scope.is_none() {
                return Err(record.refusal(LifecycleCause::Uncertain));
            }
            // Exact owner is created once. A retry cannot silently duplicate its VMM. A terminal
            // receipt is retained in this SAME slot for lost-reply/finalize retries, not another map.
            if let Ok(existing) = get(runtime, key, None) {
                return attached(&existing, key.try_clone()?);
            }
            match super::super::prepare_native(runtime, record, launch, descriptors, until) {
                Ok(boxed) => attached(&boxed, key.try_clone()?),
                Err(error) => match get(runtime, key, None) {
                    Ok(boxed) => {
                        boxed
                            .scope()
                            .ok_or_else(|| record.refusal(LifecycleCause::Uncertain))?
                            .preparation_failed(error)?;
                        attached(&boxed, key.try_clone()?)
                    }
                    Err(_) => Err(error),
                },
            }
        }
        protocol::Request::Control {
            action, controller, ..
        } => {
            if *action == protocol::Action::Inspect {
                let absent = {
                    let boxes = runtime.boxes()?;
                    crate::macos::native_owner_index(&boxes, key.owner.as_ref()).is_err()
                };
                if absent {
                    return Ok((
                        protocol::Answer::Refused {
                            key: key.try_clone()?,
                            failure: protocol::Failure::UnknownOwner,
                        },
                        None,
                    ));
                }
            }
            let boxed = get(runtime, key, *controller)?;
            match action {
                protocol::Action::Inspect => {
                    let scope = boxed.scope().ok_or_else(|| {
                        super::super::configuration("native metadata has no scope")
                    })?;
                    if let Some(error) = scope.preparation_error()? {
                        return Err(error);
                    }
                    if let Some(error) = scope.observation_error()? {
                        return Err(error);
                    }
                    let controller = boxed.identity.get().map(|identity| ProcId {
                        pid: identity.pid,
                        start_ticks: identity.started,
                    });
                    let vsock = scope
                        .vsock
                        .get()
                        .map(|vsock| copy_raw(vsock.as_os_str().as_bytes()))
                        .transpose()?
                        .ok_or_else(|| scope.refusal(boxed.owner()))?;
                    Ok((
                        protocol::Answer::Metadata {
                            key: key.try_clone()?,
                            controller,
                            vsock,
                        },
                        None,
                    ))
                }
                protocol::Action::Attach => attached(&boxed, key.try_clone()?),
                protocol::Action::Resume => {
                    if controller.is_none() {
                        return Err(super::super::configuration(
                            "native Resume requires exact controller identity",
                        ));
                    }
                    boxed.resume_managed()?;
                    Ok((
                        protocol::Answer::Done {
                            key: key.try_clone()?,
                        },
                        None,
                    ))
                }
                protocol::Action::Stop => {
                    boxed.stop_managed_raw().map_err(|_| {
                        super::super::configuration("native scope stop remained uncertain")
                    })?;
                    Ok((
                        protocol::Answer::Done {
                            key: key.try_clone()?,
                        },
                        None,
                    ))
                }
                protocol::Action::Terminal => {
                    let end = boxed.terminal().map_err(|_| {
                        super::super::configuration("native scope observation remained uncertain")
                    })?;
                    let end = end.map(|end| {
                        let exit = match end.status {
                            ManagedStatus::Code(code) => Exit::Code(code),
                            ManagedStatus::Signal(signal) => Exit::Signal(signal),
                            ManagedStatus::Unknown => Exit::Unknown,
                        };
                        match end.scope {
                            TerminalScope::Process => protocol::End::Process(exit),
                            TerminalScope::Released => protocol::End::Released(exit),
                        }
                    });
                    Ok((
                        protocol::Answer::Terminal {
                            key: key.try_clone()?,
                            end,
                        },
                        None,
                    ))
                }
                protocol::Action::Usage => {
                    let scope = boxed
                        .scope()
                        .ok_or_else(|| super::super::configuration("native usage has no scope"))?;
                    let group = boxed.group()?;
                    scope.validate(
                        group.map(|group| group.0),
                        group.map(|group| group.1),
                        boxed.owner(),
                    )?;
                    Ok((
                        protocol::Answer::Usage {
                            key: key.try_clone()?,
                            usage: boxed.usage()?,
                        },
                        None,
                    ))
                }
                protocol::Action::Finalize => {
                    super::super::finalize_native(&boxed)?;
                    Ok((
                        protocol::Answer::Done {
                            key: key.try_clone()?,
                        },
                        None,
                    ))
                }
            }
        }
    }
}

fn census(runtime: &MacRuntime, process: ProcId) -> Result<protocol::census::Census, Refusal> {
    let boxes = runtime.boxes()?;
    let mut owners = Vec::new();
    owners
        .try_reserve_exact(boxes.len())
        .map_err(|_| native_allocation(boxes.len()))?;
    for (owner, boxed) in boxes.iter() {
        let scope = boxed.scope().ok_or_else(|| {
            super::super::configuration("a rootless native helper owns an ordinary process slot")
        })?;
        if matches!(
            *scope.lock().map_err(|_| scope.refusal(owner.as_ref()))?,
            super::super::Phase::Finalized(_)
        ) {
            continue;
        }
        owners.push(protocol::Key {
            owner: owner.try_clone()?,
            scope: scope.scope,
            commitment: scope.commitment,
            helper: process,
        });
    }
    Ok(protocol::census::Census {
        helper: process,
        owners,
    })
}

fn completed(
    runtime: &MacRuntime,
    process: ProcId,
) -> Result<Option<protocol::completed::Snapshot>, Refusal> {
    let boxes = runtime.boxes()?;
    let mut owners = Vec::new();
    owners
        .try_reserve_exact(boxes.len())
        .map_err(|_| native_allocation(boxes.len()))?;
    for (owner, boxed) in boxes.iter() {
        let scope = boxed.scope().ok_or_else(|| {
            super::super::configuration("helper completion has ordinary process slot")
        })?;
        let end = match *scope.lock().map_err(|_| scope.refusal(owner.as_ref()))? {
            super::super::Phase::Finalized(end) if end.scope == TerminalScope::Released => end,
            _ => return Ok(None),
        };
        let vsock = scope
            .vsock
            .get()
            .ok_or_else(|| scope.refusal(owner.as_ref()))?;
        owners.push(protocol::completed::Completed {
            key: protocol::Key {
                owner: owner.try_clone()?,
                scope: scope.scope,
                commitment: scope.commitment,
                helper: process,
            },
            controller: boxed.identity.get().map(|identity| ProcId {
                pid: identity.pid,
                start_ticks: identity.started,
            }),
            status: match end.status {
                ManagedStatus::Code(code) => Exit::Code(code),
                ManagedStatus::Signal(signal) => Exit::Signal(signal),
                ManagedStatus::Unknown => Exit::Unknown,
            },
            vsock: copy_raw(vsock.as_os_str().as_bytes())?,
        });
    }
    Ok(Some(protocol::completed::Snapshot {
        helper: process,
        owners,
    }))
}

#[cfg(test)]
mod controls {
    use super::*;
    /// AC-T7.4.1: a failed diagnostic write retains the original request failure and the actual
    /// publication failure for the next external wake, without another native dispatch.
    #[test]
    fn failed_diagnostic_publication_retains_both_original_and_write_failure() {
        let original = Refusal::os(
            "counted sibling request",
            std::io::Error::from_raw_os_error(13),
        );
        let mut pending = PendingReply::new(original);
        pending.failed(Refusal::os(
            "receipt write",
            std::io::Error::from_raw_os_error(28),
        ));
        let detail = pending.detail().unwrap();
        let text = std::str::from_utf8(&detail.detail).unwrap();
        assert!(text.contains("counted sibling request"));
        assert!(text.contains("receipt write"));
        assert!(text.contains("attempt 1"));
        let bytes = protocol::rejected::encode(VERSION, 0, &detail).unwrap();
        assert!(protocol::read_answer(&bytes, 0).is_err());
        assert!(protocol::completed::read(&bytes, 0, 1).is_err());
        pending.failed(Refusal::os(
            "next external wake write",
            std::io::Error::from_raw_os_error(5),
        ));
        let detail = pending.detail().unwrap();
        let text = std::str::from_utf8(&detail.detail).unwrap();
        assert!(text.contains("counted sibling request"));
        assert!(text.contains("next external wake write"));
        assert!(text.contains("attempt 2"));
    }
}
