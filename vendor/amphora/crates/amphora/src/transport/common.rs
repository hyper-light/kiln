use super::{
    Error, auth,
    endpoint::{self, Stream},
    wire::*,
};
use amphora_api::{
    self as api, Records, Refusal,
    sql::{self, Session},
};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    cell::{Cell, RefCell},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

pub(super) fn unavailable(e: impl std::fmt::Display) -> Refusal {
    Refusal::Unavailable(e.to_string())
}
fn transfer_error(error: std::io::Error) -> Refusal {
    // These transfers use blocking streams with the remaining absolute deadline installed.
    // Preserve kernel timeout identity for the read-only capability phase; mutation callers
    // still map any transfer failure to UnknownOutcome after transmission begins.
    match error.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => Refusal::Timeout,
        _ => unavailable(error),
    }
}
fn sql_error(e: impl std::fmt::Display) -> sql::Error {
    sql::Error::Database(e.to_string())
}
fn session_error(error: Error) -> sql::Error {
    sql::Error::Refusal(Refusal::from(error))
}
fn sql_response(result: sql::Result<Answer>) -> Response {
    match result {
        Ok(answer) => Ok(answer),
        Err(sql::Error::Refusal(refusal)) => Err(refusal),
        Err(other) => Ok(Answer::SqlError(other.to_string())),
    }
}
pub(super) fn remaining(until: Instant) -> Result<Duration, Refusal> {
    let left = until.saturating_duration_since(Instant::now());
    if left.is_zero() {
        Err(Refusal::Timeout)
    } else {
        Ok(left)
    }
}
fn transfer(stream: &mut Stream, bytes: &mut [u8], until: Instant) -> Result<(), Refusal> {
    let mut offset = 0usize;
    while offset < bytes.len() {
        stream
            .set_read_timeout(Some(remaining(until)?))
            .map_err(unavailable)?;
        let rest = bytes
            .get_mut(offset..)
            .ok_or_else(|| unavailable("frame offset"))?;
        let n = stream.read(rest).map_err(transfer_error)?;
        if n == 0 {
            return Err(unavailable("owner connection closed"));
        }
        offset = offset
            .checked_add(n)
            .ok_or_else(|| unavailable("frame overflow"))?;
    }
    Ok(())
}
pub(super) fn read<T: DeserializeOwned>(stream: &mut Stream, until: Instant) -> Result<T, Refusal> {
    read_limit(stream, until, FRAME_MAX)
}
pub(super) fn read_limit<T: DeserializeOwned>(
    stream: &mut Stream,
    until: Instant,
    max: usize,
) -> Result<T, Refusal> {
    let mut size = [0u8; 4];
    transfer(stream, &mut size, until)?;
    let size = usize::try_from(u32::from_be_bytes(size)).map_err(unavailable)?;
    api::bound("transport frame", size, max)?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(size).map_err(unavailable)?;
    bytes.resize(size, 0);
    transfer(stream, &mut bytes, until)?;
    postcard::from_bytes(&bytes).map_err(unavailable)
}
struct FrameBuffer<'a> {
    bytes: Vec<u8>,
    max: usize,
    failure: &'a mut Option<Refusal>,
}
impl FrameBuffer<'_> {
    fn fail(&mut self, error: Refusal) -> postcard::Error {
        *self.failure = Some(error);
        postcard::Error::SerializeBufferFull
    }
}
impl postcard::ser_flavors::Flavor for FrameBuffer<'_> {
    type Output = Vec<u8>;
    fn try_push(&mut self, data: u8) -> postcard::Result<()> {
        self.try_extend(std::slice::from_ref(&data))
    }
    fn try_extend(&mut self, data: &[u8]) -> postcard::Result<()> {
        let wanted = match self.bytes.len().checked_add(data.len()) {
            Some(n) => n,
            None => return Err(self.fail(api::invalid("transport frame", "size overflow"))),
        };
        if wanted > self.max {
            return Err(self.fail(Refusal::TooLarge {
                what: "transport frame".into(),
                size: wanted,
                limit: self.max,
            }));
        }
        if wanted > self.bytes.capacity() {
            let doubled = self.bytes.capacity().checked_mul(2).unwrap_or(self.max);
            let target = doubled.max(wanted).max(128).min(self.max);
            let extra = target
                .checked_sub(self.bytes.len())
                .ok_or(postcard::Error::SerializeBufferFull)?;
            if let Err(error) = self.bytes.try_reserve_exact(extra) {
                return Err(self.fail(unavailable(error)));
            }
        }
        self.bytes.extend_from_slice(data);
        Ok(())
    }
    fn finalize(self) -> postcard::Result<Self::Output> {
        Ok(self.bytes)
    }
}
pub(super) fn encode<T: Serialize>(value: &T, max: usize) -> Result<Vec<u8>, Refusal> {
    let mut failure = None;
    postcard::serialize_with_flavor(
        value,
        FrameBuffer {
            bytes: Vec::new(),
            max,
            failure: &mut failure,
        },
    )
    .map_err(|error| failure.unwrap_or_else(|| unavailable(error)))
}
pub(super) fn send(stream: &mut Stream, encoded: &[u8], until: Instant) -> Result<(), Refusal> {
    let size = u32::try_from(encoded.len())
        .map_err(unavailable)?
        .to_be_bytes();
    for mut part in [size.as_slice(), encoded] {
        while !part.is_empty() {
            stream
                .set_write_timeout(Some(remaining(until)?))
                .map_err(unavailable)?;
            let n = stream.write(part).map_err(transfer_error)?;
            if n == 0 {
                return Err(unavailable("owner connection closed"));
            }
            part = part.get(n..).ok_or_else(|| unavailable("frame offset"))?;
        }
    }
    Ok(())
}
fn write<T: Serialize>(stream: &mut Stream, value: &T, until: Instant) -> Result<(), Refusal> {
    send(stream, &encode(value, FRAME_MAX)?, until)
}
fn write_auth<T: Serialize>(stream: &mut Stream, value: &T, until: Instant) -> Result<(), Refusal> {
    send(stream, &encode(value, 256)?, until)
}
struct Permit(Arc<AtomicUsize>);
impl Permit {
    fn take(count: &Arc<AtomicUsize>, max: usize) -> Result<Self, Refusal> {
        count
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                if n < max { n.checked_add(1) } else { None }
            })
            .map_err(|_| Refusal::Full { limit: max })?;
        Ok(Self(Arc::clone(count)))
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Stored once in the database's shared owner, never on individual clone handles.
pub struct Owner {
    stop: Arc<AtomicBool>,
    listener: Option<thread::JoinHandle<()>>,
}
impl Owner {
    /// `acquire` must upgrade a weak database reference, avoiding an owner/database cycle.
    /// The caller must hold the database's OS ownership lock before invoking this method.
    pub fn start(
        path: &Path,
        acquire: impl Fn() -> Option<crate::database::Database> + Send + Sync + 'static,
    ) -> Result<Self, Refusal> {
        let endpoint::Bound {
            listener,
            token,
            guard,
        } = endpoint::bind(path)?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let acquire = Arc::new(acquire);
        let count = Arc::new(AtomicUsize::new(0));
        let waits = Arc::new(AtomicUsize::new(0));
        let handle = thread::Builder::new()
            .name("amphora.owner".into())
            .spawn(move || {
                let _guard = guard;
                while !stopping.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            if let Err(error)=stream.set_nonblocking(false) {
                                tracing::warn!(target:"amphora.transport",event="stream_configuration_failed",%error);
                                continue;
                            }
                            if let Err(error)=stream.set_nodelay(true) {
                                tracing::warn!(target:"amphora.transport",event="stream_configuration_failed",%error);
                                continue;
                            }
                            let Ok(permit) = Permit::take(&count, CONNECTION_MAX) else {
                                tracing::debug!(target: "amphora.transport", event = "connection_refused", reason = "connection_limit");
                                drop(stream);
                                continue;
                            };
                            let acquire = Arc::clone(&acquire);
                            let waits = Arc::clone(&waits);
                            // Detached workers are bounded by permits and the absolute session deadline.
                            let spawned = thread::Builder::new()
                                .name("amphora.client".into())
                                .spawn(move || {
                                    let _permit = permit;
                                    if let Err(error) = serve(stream, token, &*acquire, &waits) {
                                        tracing::debug!(target: "amphora.transport", event = "connection_finished", %error);
                                    }
                                });
                            if let Err(error) = spawned {
                                tracing::warn!(target: "amphora.transport", event = "worker_start_failed", %error);
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::park_timeout(Duration::from_millis(10))
                        }
                        Err(error) => {
                            tracing::error!(target: "amphora.transport", event = "listener_failed", %error);
                            break;
                        }
                    }
                }
            })
            .map_err(unavailable)?;
        Ok(Self {
            stop,
            listener: Some(handle),
        })
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.listener.take() {
            handle.thread().unpark();
            let _joined = handle.join();
        }
    }
}

#[derive(Clone)]
pub struct Client {
    pub(super) path: PathBuf,
}
impl Client {
    pub fn connect(path: &Path, until: Instant) -> Result<Self, Refusal> {
        remaining(until)?;
        let client = Self {
            path: path.canonicalize().map_err(unavailable)?,
        };
        // Authenticate now, without creating a writer session or replaying any command.
        let mut stream = client.stream(until)?;
        write(&mut stream, &Request::Probe, until)?;
        let response: Response = read(&mut stream, until)?;
        match response? {
            Answer::Done => (),
            _ => return Err(unavailable("unexpected owner probe response")),
        }
        Ok(client)
    }
    pub(super) fn stream(&self, until: Instant) -> Result<Stream, Refusal> {
        remaining(until)?;
        let auth_until = until.min(
            Instant::now()
                .checked_add(Duration::from_secs(2))
                .ok_or(Refusal::Timeout)?,
        );
        let (mut stream, token) = endpoint::connect(&self.path, auth_until)?;
        let millis =
            u64::try_from(remaining(until)?.min(SESSION_MAX).as_millis()).map_err(unavailable)?;
        let hello = Hello {
            version: 2,
            nonce: auth::nonce()?,
            millis,
        };
        write_auth(&mut stream, &hello, auth_until)?;
        let challenge: Challenge = read_limit(&mut stream, auth_until, 256)?;
        auth::verify(&token, true, &hello, &challenge.nonce, &challenge.proof)?;
        let proof = auth::proof(&token, false, &hello, &challenge.nonce)?;
        write_auth(&mut stream, &proof, auth_until)?;
        match read_limit::<Response>(&mut stream, auth_until, 256)?? {
            Answer::Ready => Ok(stream),
            _ => Err(unavailable("unexpected owner greeting")),
        }
    }
    pub(super) fn request(&self, request: Request, until: Instant) -> Result<Answer, Error> {
        let encoded = encode(&request, FRAME_MAX)?;
        let mut stream = self.stream(until)?;
        send(&mut stream, &encoded, until).map_err(|_| Error::UnknownOutcome)?;
        read::<Response>(&mut stream, until)
            .map_err(|_| Error::UnknownOutcome)?
            .map_err(Error::from)
    }
    fn compact_step(
        &self,
        namespace: &str,
        before: api::Rev,
        until: Instant,
    ) -> Result<api::CompactionProgress, Refusal> {
        let mut stream = self.stream(until)?;
        compact_step_on(&mut stream, namespace, before, until)
    }
    pub fn execute(
        &self,
        statements: Vec<api::Statement>,
        until: Instant,
    ) -> Result<Vec<api::SqlResult>, Error> {
        match self.request(Request::Execute(statements), until)? {
            Answer::Sql(rows) => Ok(rows),
            _ => Err(Error::UnknownOutcome),
        }
    }
    pub fn migrate(&self, migration: api::Migration, until: Instant) -> Result<bool, Error> {
        match self.request(Request::Migrate(migration), until)? {
            Answer::Migrated(done) => Ok(done),
            _ => Err(Error::UnknownOutcome),
        }
    }
    pub(crate) fn identity(
        &self,
        until: Instant,
    ) -> Result<crate::database::remote::Identity, Error> {
        match self.request(Request::Identity, until)? {
            Answer::Identity(identity) => Ok(identity),
            _ => Err(unavailable("unexpected owner identity response").into()),
        }
    }
    pub(crate) fn replica_status(
        &self,
        until: Instant,
    ) -> Result<Option<api::replication::ReplicaStatus>, Error> {
        match self.request(Request::ReplicaStatus, until)? {
            Answer::ReplicaStatus(status) => Ok(status),
            _ => Err(unavailable("unexpected owner replication status response").into()),
        }
    }
    pub fn restore_progress(
        &self,
        until: Instant,
    ) -> Result<Option<api::import::ImportProgress>, Error> {
        match self.request(Request::RestoreProgress, until)? {
            Answer::ImportState(progress) => Ok(progress),
            _ => Err(unavailable("unexpected restore progress response").into()),
        }
    }
    pub fn namespace(&self, name: &str) -> Result<Namespace, Refusal> {
        api::family(name)?;
        Ok(Namespace {
            client: self.clone(),
            name: name.to_owned(),
        })
    }
    pub fn call<R, F>(&self, until: Instant, f: F) -> Result<R, Error>
    where
        F: FnOnce(&mut sql::Connection<'_>) -> anyhow::Result<R>,
    {
        let mut stream = self.stream(until)?;
        write(&mut stream, &Request::Session, until)?;
        match read::<Response>(&mut stream, until)?? {
            Answer::Ready => (),
            _ => return Err(unavailable("session not ready").into()),
        }
        let session = Remote {
            stream: RefCell::new(stream),
            until,
            count: Cell::new(0),
            changes: Cell::new(0),
            rowid: Cell::new(0),
            failed: Cell::new(false),
        };
        let mut connection = sql::Connection::new(&session);
        let value = match f(&mut connection).and_then(|v| {
            connection.finish()?;
            Ok(v)
        }) {
            Ok(value) if !session.failed.get() => value,
            result => {
                let _abort = session.exchange(Command::Abort);
                return Err(match result {
                    Err(error) => Error::Refused(crate::database::refused(&error)),
                    Ok(_) => Error::UnknownOutcome,
                });
            }
        };
        match session.exchange(Command::Commit) {
            Ok(Answer::Done) => Ok(value),
            Err(error) => Err(error),
            _ => Err(Error::UnknownOutcome),
        }
    }
}
struct Remote {
    stream: RefCell<Stream>,
    until: Instant,
    count: Cell<usize>,
    changes: Cell<i64>,
    rowid: Cell<i64>,
    failed: Cell<bool>,
}
impl Remote {
    fn command(&self, command: Command) -> sql::Result<Answer> {
        match self.exchange(command).map_err(session_error)? {
            Answer::SqlError(message) => Err(sql::Error::Database(message)),
            answer => Ok(answer),
        }
    }
    fn unexpected(&self) -> sql::Error {
        self.failed.set(true);
        sql::Error::Refusal(Refusal::UnknownOutcome)
    }
    fn exchange(&self, command: Command) -> Result<Answer, Error> {
        let n = self
            .count
            .get()
            .checked_add(1)
            .ok_or_else(|| unavailable("session budget overflow"))?;
        api::bound("session commands", n, COMMAND_MAX)?;
        self.count.set(n);
        let mut stream = self.stream.try_borrow_mut().map_err(unavailable)?;
        let result = (|| {
            let encoded = encode(&command, FRAME_MAX)?;
            send(&mut stream, &encoded, self.until).map_err(|_| Error::UnknownOutcome)?;
            read::<Response>(&mut stream, self.until)
                .map_err(|_| Error::UnknownOutcome)?
                .map_err(Error::from)
        })();
        if matches!(result, Err(Error::UnknownOutcome)) {
            self.failed.set(true)
        }
        result
    }
}
impl Session for Remote {
    fn records_get(
        &self,
        namespace: &str,
        family: &str,
        key: &[u8],
    ) -> sql::Result<Option<api::Row>> {
        api::family(namespace)?;
        api::family(family)?;
        api::key(key)?;
        match self.command(Command::RecordsGet(
            namespace.into(),
            family.into(),
            key.into(),
        ))? {
            Answer::SessionRow {
                row,
                changes,
                rowid,
            } => {
                self.changes.set(changes);
                self.rowid.set(rowid);
                Ok(row)
            }
            _ => Err(self.unexpected()),
        }
    }
    fn records_commit(&self, namespace: &str, tx: api::Tx) -> sql::Result<api::Rev> {
        api::family(namespace)?;
        api::transaction(&tx)?;
        match self.command(Command::RecordsCommit(namespace.into(), tx))? {
            Answer::SessionRev {
                rev,
                changes,
                rowid,
            } => {
                self.changes.set(changes);
                self.rowid.set(rowid);
                Ok(rev)
            }
            _ => Err(self.unexpected()),
        }
    }
    fn execute_batch(&self, sql: &str) -> sql::Result<()> {
        api::bound("SQL bytes", sql.len(), api::SQL_MAX).map_err(sql_error)?;
        match self.command(Command::Batch(sql.to_owned()))? {
            Answer::Rows { changes, rowid, .. } => {
                self.changes.set(changes);
                self.rowid.set(rowid);
                Ok(())
            }
            _ => Err(self.unexpected()),
        }
    }

    fn query(
        &self,
        sql: &str,
        params: Vec<api::Value>,
        first: bool,
    ) -> sql::Result<Vec<Vec<api::Value>>> {
        api::bound("SQL bytes", sql.len(), api::SQL_MAX).map_err(sql_error)?;
        match self.command(Command::Query(sql.to_owned(), params, first))? {
            Answer::Rows {
                rows,
                changes,
                rowid,
            } => {
                self.changes.set(changes);
                self.rowid.set(rowid);
                Ok(rows)
            }
            _ => Err(self.unexpected()),
        }
    }
    fn column_count(&self, sql: &str) -> sql::Result<usize> {
        api::bound("SQL bytes", sql.len(), api::SQL_MAX).map_err(sql_error)?;
        match self.command(Command::Columns(sql.to_owned()))? {
            Answer::Columns(n) => Ok(n),
            _ => Err(self.unexpected()),
        }
    }
    fn changes(&self) -> i64 {
        self.changes.get()
    }
    fn last_insert_rowid(&self) -> i64 {
        self.rowid.get()
    }
}

fn capability_unverified(error: Refusal) -> Refusal {
    match error {
        Refusal::Timeout => Refusal::Timeout,
        _ => api::invalid(
            "bounded compaction capability",
            "support not verified; no mutation sent",
        ),
    }
}

fn compact_step_on(
    stream: &mut Stream,
    namespace: &str,
    before: api::Rev,
    until: Instant,
) -> Result<api::CompactionProgress, Refusal> {
    // Probe and mutation share one authenticated stream: an owner replacement cannot invalidate
    // a cached affirmative capability between connections. This probe acquires no writer.
    let probe = encode(&Request::Capabilities, 256)?;
    send(stream, &probe, until).map_err(|error| {
        if Instant::now() >= until {
            Refusal::Timeout
        } else {
            capability_unverified(error)
        }
    })?;
    let response: Response = read_limit(stream, until, 256).map_err(|error| {
        if Instant::now() >= until {
            Refusal::Timeout
        } else {
            capability_unverified(error)
        }
    })?;
    match response? {
        Answer::Capabilities(bits) if bits & CAP_BOUNDED_COMPACTION != 0 => (),
        _ => return Err(capability_unverified(unavailable("unsupported capability"))),
    }
    let request = Request::Records {
        namespace: namespace.into(),
        op: Record::CompactStep(before),
    };
    let encoded = encode(&request, 256)?;
    // Once mutation transmission begins, loss/decode failure is conservatively unknown outcome.
    send(stream, &encoded, until).map_err(|_| Refusal::UnknownOutcome)?;
    match read::<Response>(stream, until).map_err(|_| Refusal::UnknownOutcome)?? {
        Answer::Compaction(progress) => Ok(progress),
        _ => Err(Refusal::UnknownOutcome),
    }
}

fn serve(
    mut stream: Stream,
    token: [u8; 32],
    acquire: &dyn Fn() -> Option<crate::database::Database>,
    waits: &Arc<AtomicUsize>,
) -> Result<(), Refusal> {
    let started = Instant::now();
    let greeting_until = started
        .checked_add(Duration::from_secs(2))
        .ok_or(Refusal::Timeout)?;
    let hello: Hello = read_limit(&mut stream, greeting_until, 256)?;
    if hello.version != 2 || hello.millis == 0 || Duration::from_millis(hello.millis) > SESSION_MAX
    {
        return Err(api::invalid("owner greeting", "version or deadline"));
    }
    let nonce = auth::nonce()?;
    write_auth(
        &mut stream,
        &Challenge {
            nonce,
            proof: auth::proof(&token, true, &hello, &nonce)?,
        },
        greeting_until,
    )?;
    let proof: [u8; 32] = read_limit(&mut stream, greeting_until, 256)?;
    auth::verify(&token, false, &hello, &nonce, &proof)?;
    let until = started
        .checked_add(Duration::from_millis(hello.millis))
        .ok_or(Refusal::Timeout)?;
    write(&mut stream, &Ok::<_, Refusal>(Answer::Ready), until)?;
    let mut request: Request = read(&mut stream, until)?;
    if matches!(request, Request::Capabilities) {
        write(
            &mut stream,
            &Ok::<_, Refusal>(Answer::Capabilities(CAP_BOUNDED_COMPACTION)),
            until,
        )?;
        // One small, restricted continuation; EOF is harmless and still acquires no database.
        request = read_limit(&mut stream, until, 256)?;
        if !matches!(
            request,
            Request::Records {
                op: Record::CompactStep(_),
                ..
            }
        ) {
            return Err(api::invalid(
                "capability continuation",
                "expected one bounded compaction step",
            ));
        }
    }
    if matches!(request, Request::Probe) {
        return write(&mut stream, &Ok::<_, Refusal>(Answer::Done), until);
    }
    let database = acquire().ok_or_else(|| unavailable("owner is draining"))?;
    if matches!(request, Request::Session) {
        let mut final_stream = stream.try_clone().map_err(unavailable)?;
        let result = database
            .call(until, move |connection| {
                write(&mut stream, &Ok::<_, Refusal>(Answer::Ready), until)?;
                for _ in 0..COMMAND_MAX {
                    let command: Command = read(&mut stream, until)?;
                    let response = match command {
                        Command::Commit => {
                            remaining(until)?;
                            return Ok(());
                        }
                        Command::Abort => {
                            return Err(anyhow::anyhow!("client aborted SQL session"));
                        }
                        Command::Batch(sql) => connection.execute_batch(&sql).and_then(|()| {
                            Ok(Answer::Rows {
                                rows: Vec::new(),
                                changes: i64::try_from(connection.changes()).map_err(sql_error)?,
                                rowid: connection.last_insert_rowid(),
                            })
                        }),
                        Command::Columns(sql) => connection
                            .prepare(&sql)
                            .map(|s| Answer::Columns(s.column_count())),
                        Command::Query(sql, params, first) => {
                            query(connection, &sql, params, first)
                        }
                        Command::RecordsGet(namespace, family, key) => connection
                            .records_get(&namespace, &family, &key)
                            .and_then(|row| {
                                Ok(Answer::SessionRow {
                                    row,
                                    changes: i64::try_from(connection.changes())
                                        .map_err(sql_error)?,
                                    rowid: connection.last_insert_rowid(),
                                })
                            }),
                        Command::RecordsCommit(namespace, tx) => {
                            connection.records_commit(&namespace, tx).and_then(|rev| {
                                Ok(Answer::SessionRev {
                                    rev,
                                    changes: i64::try_from(connection.changes())
                                        .map_err(sql_error)?,
                                    rowid: connection.last_insert_rowid(),
                                })
                            })
                        }
                    };
                    let response = sql_response(response);
                    write(&mut stream, &response, until)?;
                    // SQL errors can be handled by application savepoints; transport failures abort.
                }
                Err(anyhow::anyhow!("SQL session command budget exhausted"))
            })
            .map(|()| Answer::Done);
        return write(&mut final_stream, &result, until);
    }
    let watch_batch = matches!(&request, Request::WatchBatch(_));
    let result = match request {
        Request::Execute(statements) => database.execute(statements, until).map(Answer::Sql),
        Request::Migrate(migration) => database.migrate(migration, until).map(Answer::Migrated),
        Request::Identity => database.transport_identity().map(Answer::Identity),
        Request::ReplicaStatus => database.replica_status(until).map(Answer::ReplicaStatus),
        Request::RestoreProgress => database.restore_progress(until).map(Answer::ImportState),
        Request::WatchBatch(requests) => {
            api::bound("watch batch", requests.len(), WATCH_BATCH_MAX)?;
            Ok(Answer::WatchBatch(
                requests
                    .into_iter()
                    .map(|(namespace, family, after)| {
                        database
                            .namespace(&namespace)?
                            .try_wait_selected(&family, after, until)
                    })
                    .collect(),
            ))
        }
        Request::Records { namespace, op } => (|| {
            let ns = database.namespace(&namespace)?;
            Ok(match op {
                Record::Get(f, k, at) => Answer::Row(ns.get(&f, &k, at, until)?),
                Record::Scan(f, s) => Answer::Page(ns.scan(&f, s, until)?),
                Record::Commit(tx) => Answer::Rev(ns.commit(tx, until)?),
                Record::Changes(f, r, n) => Answer::Changes(ns.changes(&f, r, n, until)?),
                Record::Wait(f, r) => {
                    let _permit = Permit::take(waits, WAIT_MAX)?;
                    Answer::Rev(wait_remote(&ns, &f, r, &mut stream, until)?)
                }
                Record::Head => Answer::Rev(ns.head(until)?),
                Record::CompactStep(before) => Answer::Compaction(ns.compact_step(before, until)?),
                Record::Compact(r) => {
                    ns.compact(r, until)?;
                    Answer::Done
                }
                Record::TryWait(f, r) => Answer::OptionalRev(ns.try_wait(&f, r)?),
                Record::ImportPage(page) => {
                    ns.import_page(page, until)?;
                    Answer::Done
                }
                Record::ImportState => Answer::ImportState(ns.import_state(until)?),
            })
        })(),
        Request::Session | Request::Probe | Request::Capabilities => {
            Err(unavailable("unexpected session"))
        }
    };
    if watch_batch {
        let encoded = match encode(&result, WATCH_REPLY_MAX) {
            Ok(encoded) => encoded,
            Err(error) => encode(&Err::<Answer, _>(error), WATCH_REPLY_MAX)?,
        };
        send(&mut stream, &encoded, until)
    } else {
        write(&mut stream, &result, until)
    }
}
struct WaitingThread(std::thread::Thread);
impl std::task::Wake for WaitingThread {
    fn wake(self: Arc<Self>) {
        self.0.unpark()
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark()
    }
}
fn wait_remote(
    namespace: &crate::database::Namespace,
    family: &str,
    after: api::Rev,
    stream: &mut Stream,
    until: Instant,
) -> Result<api::Rev, Refusal> {
    use std::{
        future::Future,
        task::{Context, Poll, Waker},
    };
    let waker = Waker::from(Arc::new(WaitingThread(thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut watch = Box::pin(namespace.watch(family, after)?);
    stream.set_nonblocking(true).map_err(unavailable)?;
    let result = (|| {
        loop {
            remaining(until)?;
            if let Poll::Ready(result) = watch.as_mut().poll(&mut cx) {
                return result;
            }
            match stream.peek(&mut [0u8; 1]) {
                Ok(0) => return Err(unavailable("client cancelled Records wait")),
                Ok(_) => return Err(api::invalid("Records wait", "unexpected pipelined bytes")),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => (),
                Err(error) => return Err(unavailable(error)),
            }
            // Commit notifications unpark immediately. This administrative 10ms bound only
            // limits how long a disconnected peer can retain one of the eight wait slots.
            thread::park_timeout(remaining(until)?.min(Duration::from_millis(10)));
        }
    })();
    stream.set_nonblocking(false).map_err(unavailable)?;
    result
}

fn query(
    connection: &sql::Connection<'_>,
    sql: &str,
    params: Vec<api::Value>,
    first: bool,
) -> sql::Result<Answer> {
    let rows = if first {
        match connection.query_row(sql, params, |row| {
            let mut values = Vec::new();
            for column in 0..row.column_count() {
                values.push(row.get::<_, api::Value>(column)?);
            }
            Ok(values)
        }) {
            Ok(values) => vec![values],
            Err(sql::Error::QueryReturnedNoRows) => Vec::new(),
            Err(error) => return Err(error),
        }
    } else {
        let mut statement = connection.prepare(sql)?;
        let columns = statement.column_count();
        let mut cursor = statement.query(params)?;
        let mut rows = Vec::new();
        while let Some(row) = cursor.next()? {
            let mut values = Vec::new();
            for column in 0..columns {
                values.push(row.get::<_, api::Value>(column)?);
            }
            rows.push(values);
        }
        rows
    };
    Ok(Answer::Rows {
        rows,
        changes: i64::try_from(connection.changes()).map_err(sql_error)?,
        rowid: connection.last_insert_rowid(),
    })
}

#[derive(Clone)]
pub struct Namespace {
    pub(super) client: Client,
    pub(super) name: String,
}
impl Namespace {
    pub fn try_wait(
        &self,
        family: &str,
        after: api::Rev,
        until: Instant,
    ) -> Result<Option<api::Rev>, Refusal> {
        api::family(family)?;
        match self.request(Record::TryWait(family.into(), after), until)? {
            Answer::OptionalRev(rev) => Ok(rev),
            _ => Err(unavailable("unexpected wait readiness response")),
        }
    }
    pub fn import_page(
        &self,
        page: api::import::ImportPage,
        until: Instant,
    ) -> Result<(), Refusal> {
        api::bound("import versions", page.versions.len(), api::TX_MAX)?;
        api::bound("import families", page.state.newest.len(), api::TX_MAX)?;
        match self.request(Record::ImportPage(page), until)? {
            Answer::Done => Ok(()),
            _ => Err(Refusal::UnknownOutcome),
        }
    }
    pub fn import_state(
        &self,
        until: Instant,
    ) -> Result<Option<api::import::ImportProgress>, Refusal> {
        match self.request(Record::ImportState, until)? {
            Answer::ImportState(progress) => Ok(progress),
            _ => Err(unavailable("unexpected import progress response")),
        }
    }
    fn request(&self, op: Record, until: Instant) -> Result<Answer, Refusal> {
        self.client
            .request(
                Request::Records {
                    namespace: self.name.clone(),
                    op,
                },
                until,
            )
            .map_err(Refusal::from)
    }
}
impl Records for Namespace {
    fn get(
        &self,
        f: &str,
        k: &[u8],
        at: api::Consistency,
        u: Instant,
    ) -> Result<Option<api::Row>, Refusal> {
        api::family(f)?;
        api::key(k)?;
        match self.request(Record::Get(f.into(), k.into(), at), u)? {
            Answer::Row(v) => Ok(v),
            _ => Err(unavailable("unexpected row response")),
        }
    }
    fn scan(&self, f: &str, s: api::Scan, u: Instant) -> Result<api::Page, Refusal> {
        api::family(f)?;
        match self.request(Record::Scan(f.into(), s), u)? {
            Answer::Page(v) => Ok(v),
            _ => Err(unavailable("unexpected scan response")),
        }
    }
    fn commit(&self, t: api::Tx, u: Instant) -> Result<api::Rev, Refusal> {
        api::transaction(&t)?;
        match self.request(Record::Commit(t), u)? {
            Answer::Rev(v) => Ok(v),
            _ => Err(Refusal::UnknownOutcome),
        }
    }
    fn changes(&self, f: &str, r: api::Rev, n: usize, u: Instant) -> Result<api::Changes, Refusal> {
        api::family(f)?;
        match self.request(Record::Changes(f.into(), r, n), u)? {
            Answer::Changes(v) => Ok(v),
            _ => Err(unavailable("unexpected changes response")),
        }
    }
    fn wait(&self, f: &str, r: api::Rev, u: Instant) -> Result<api::Rev, Refusal> {
        api::family(f)?;
        match self.request(Record::Wait(f.into(), r), u)? {
            Answer::Rev(v) => Ok(v),
            _ => Err(unavailable("unexpected wait response")),
        }
    }
    fn head(&self, u: Instant) -> Result<api::Rev, Refusal> {
        match self.request(Record::Head, u)? {
            Answer::Rev(v) => Ok(v),
            _ => Err(unavailable("unexpected head response")),
        }
    }
    fn compact(&self, r: api::Rev, u: Instant) -> Result<(), Refusal> {
        api::compact(self, r, u)
    }
    fn compact_step(
        &self,
        before: api::Rev,
        until: Instant,
    ) -> Result<api::CompactionProgress, Refusal> {
        self.client.compact_step(&self.name, before, until)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pair() -> (Stream, Stream) {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let client = std::net::TcpStream::connect_timeout(
            &listener.local_addr().unwrap(),
            Duration::from_secs(2),
        )
        .unwrap();
        let (server, _) = listener.accept().unwrap();
        (client, server)
    }
    fn until() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }

    #[test]
    fn frame_length_is_refused_before_reading_payload() {
        // CP-T1.5: hostile framing cannot cause an allocation beyond the admission budget.
        let (mut sender, mut receiver) = pair();
        sender.write_all(&u32::MAX.to_be_bytes()).unwrap();
        assert!(matches!(
            read::<Request>(&mut receiver, until()),
            Err(Refusal::TooLarge { .. })
        ));

        // Existing indexes stay fixed; additive capability negotiation does not renumber v2.
        assert_eq!(
            postcard::to_allocvec(&Request::ReplicaStatus).unwrap(),
            vec![8]
        );
        assert_eq!(
            postcard::to_allocvec(&Record::ImportState).unwrap(),
            vec![9]
        );
        assert_eq!(
            postcard::to_allocvec(&Answer::SqlError(String::new())).unwrap(),
            vec![17, 0]
        );

        // A legacy-shaped owner closes on the appended probe. No mutation is sent; losing the
        // connection after an affirmative probe and a submitted step is instead unknown outcome.
        for supported in [false, true] {
            let (mut client, mut server) = pair();
            let fake = thread::spawn(move || {
                assert!(matches!(
                    read::<Request>(&mut server, until()).unwrap(),
                    Request::Capabilities
                ));
                if supported {
                    write(
                        &mut server,
                        &Ok::<_, Refusal>(Answer::Capabilities(CAP_BOUNDED_COMPACTION)),
                        until(),
                    )
                    .unwrap();
                    assert!(matches!(
                        read::<Request>(&mut server, until()).unwrap(),
                        Request::Records {
                            op: Record::CompactStep(3),
                            ..
                        }
                    ));
                } else {
                    server.shutdown(std::net::Shutdown::Write).unwrap();
                    let mut trailing = [0; 1];
                    // Client exits the read-only phase and closes without sending a mutation.
                    assert_eq!(server.read(&mut trailing).unwrap(), 0);
                }
            });
            let result = compact_step_on(&mut client, "items", 3, until());
            if supported {
                assert_eq!(result, Err(Refusal::UnknownOutcome));
            } else {
                assert!(matches!(result, Err(Refusal::Invalid { .. })));
            }
            drop(client);
            fake.join().unwrap();
        }
    }

    #[test]
    fn stale_record_cannot_authenticate_a_reused_port() {
        // CP-T1.5/U17: another loopback listener cannot impersonate a dead owner or elicit
        // a client proof merely because its port matches a protected stale record.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("db");
        std::fs::File::create(&path).unwrap();
        let owner = endpoint::bind(&path).unwrap();
        let address = owner.listener.local_addr().unwrap();
        drop(owner);
        let listener = std::net::TcpListener::bind(address).unwrap();
        let fake = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _hello: Hello = read_limit(&mut stream, until(), 256).unwrap();
            write_auth(
                &mut stream,
                &Challenge {
                    nonce: [8; 32],
                    proof: [0; 32],
                },
                until(),
            )
            .unwrap();
            assert!(read_limit::<[u8; 32]>(&mut stream, until(), 256).is_err());
        });
        assert!(matches!(
            Client::connect(&path, until()),
            Err(Refusal::Invalid { .. })
        ));
        fake.join().unwrap();
    }

    #[test]
    fn wrong_capability_never_acquires_database() {
        // U05/U06: authentication precedes writer/Records access.
        let (mut client, server) = pair();
        let accessed = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&accessed);
        let task = thread::spawn(move || {
            serve(
                server,
                [1; 32],
                &|| {
                    observed.fetch_add(1, Ordering::AcqRel);
                    None
                },
                &Arc::new(AtomicUsize::new(0)),
            )
        });
        write(
            &mut client,
            &Hello {
                version: 2,
                nonce: [2; 32],
                millis: 1000,
            },
            until(),
        )
        .unwrap();
        let challenge: Challenge = read(&mut client, until()).unwrap();
        let hello = Hello {
            version: 2,
            nonce: [2; 32],
            millis: 1000,
        };
        let proof = auth::proof(&[9; 32], false, &hello, &challenge.nonce).unwrap();
        write(&mut client, &proof, until()).unwrap();
        assert!(matches!(task.join().unwrap(), Err(Refusal::Invalid { .. })));
        assert_eq!(accessed.load(Ordering::Acquire), 0);

        let dir = tempfile::tempdir().unwrap();
        let db =
            crate::database::Database::open(&dir.path().join("compaction.db"), until()).unwrap();
        for probe_only in [true, false] {
            let (mut client, server) = pair();
            let accessed = Arc::new(AtomicUsize::new(0));
            let observed = Arc::clone(&accessed);
            let db = db.clone();
            let task = thread::spawn(move || {
                serve(
                    server,
                    [1; 32],
                    &|| {
                        observed.fetch_add(1, Ordering::AcqRel);
                        Some(db.clone())
                    },
                    &Arc::new(AtomicUsize::new(0)),
                )
            });
            let hello = Hello {
                version: 2,
                nonce: [2; 32],
                millis: 1000,
            };
            write(&mut client, &hello, until()).unwrap();
            let challenge: Challenge = read(&mut client, until()).unwrap();
            let proof = auth::proof(&[1; 32], false, &hello, &challenge.nonce).unwrap();
            write(&mut client, &proof, until()).unwrap();
            assert!(matches!(
                read::<Response>(&mut client, until()).unwrap().unwrap(),
                Answer::Ready
            ));
            if probe_only {
                write(&mut client, &Request::Capabilities, until()).unwrap();
                assert!(matches!(
                    read::<Response>(&mut client, until()).unwrap().unwrap(),
                    Answer::Capabilities(CAP_BOUNDED_COMPACTION)
                ));
                drop(client);
                assert!(task.join().unwrap().is_err());
                assert_eq!(accessed.load(Ordering::Acquire), 0);
            } else {
                let progress = compact_step_on(&mut client, "records", 0, until()).unwrap();
                assert_eq!(progress.completed, 0);
                assert!(!progress.more);
                assert!(task.join().unwrap().is_ok());
                assert_eq!(accessed.load(Ordering::Acquire), 1);
            }
        }
    }
}
