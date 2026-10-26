//! Code mode's sandbox: QuickJS-NG (rquickjs) inside kiln, with hard bounds.
//!
//! A program is the body of an async function. Operations are async functions on `kiln`
//! (generated from the registry: `kiln.cd.apps.list`, `kiln.ci.runs.list`, on a runner
//! `kiln.machines.run`) and on `kiln.call(name, input)`. A call queues a promise; the driver here
//! runs the engine's pending jobs, starts every queued call (up to [`Limits::parallel`] at once,
//! each on a thread of its own, so `Promise.all` over machines runs them side by side), settles
//! each promise as its call ends and goes on, so the engine never calls back into kiln and an
//! operation never runs on the engine's stack. What a call goes to is a [`Host`]: kiln's
//! operations on the server ([`run`]), or a runner's own and the server's through it
//! (`crate::reach::mcp`). Only what the program returns leaves the sandbox.
//!
//! Bounds, each a refusal with its name: the heap (`heap`), the wall clock (`time`, checked by
//! the interrupt handler on function calls and backward jumps), the stack (`stack`), the number
//! of calls (`calls`), the size of an answer a call gives or the program returns (`result`), a
//! program that awaits what never settles (`unsettled`), a program that doesn't parse or throws
//! (`program`). There is no I/O but `kiln`, no timers, no modules.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rquickjs::{Context, Runtime};
use serde_json::{Value, json};

use crate::ops::{self, Cx};

/// What a program's calls go to.
pub trait Host: Send + Sync + 'static {
    /// Every operation a program may call, by name (`cd.apps.list`, `machines.run`).
    fn names(&self) -> Vec<String>;
    /// The operations as `code_search` programs see them: `{name, call, summary, read_only,
    /// destructive, input}` each.
    fn registry(&self) -> Value;
    /// Run one call. It may block: the driver calls it on a thread of its own. `Err` is what the
    /// call's promise rejects with (`{"error": …}`).
    fn call(&self, name: &str, input: Value) -> Result<Value, Value>;
    /// The program ended with calls still running: stop what they started, where that means
    /// anything.
    fn abandon(&self) {}
}

/// kiln's operations on the server, for `cx`'s caller.
struct Ops(Cx);

impl Host for Ops {
    fn names(&self) -> Vec<String> {
        ops::all(&self.0.hub).iter().map(|o| o.name.to_owned()).collect()
    }

    fn registry(&self) -> Value {
        registry(&self.0.hub)
    }

    fn call(&self, name: &str, mut input: Value) -> Result<Value, Value> {
        // A program's wait hands over no messages unless it says so (`agents.wait`): what it
        // returns of the answer is its own, and a message it left out would be lost.
        if name == "agents.wait"
            && let Some(o) = input.as_object_mut()
        {
            o.entry("hand_over").or_insert(Value::Bool(false));
        }
        let (cx, name) = (self.0.clone(), name.to_owned());
        match crate::rt::block_on(async move { Ok(ops::call(cx, &name, input).await) }) {
            Ok(Ok(v)) => Ok(v),
            Ok(Err((status, msg))) => Err(json!({"error": msg, "status": status.as_u16()})),
            Err(e) => Err(json!({"error": format!("the call could not run: {e:#}")})),
        }
    }
}

/// What a program sees.
pub enum Mode {
    /// The registry, as data.
    Search,
    /// The `kiln` host object, and `input`.
    Run {
        /// Any JSON, as `input`.
        input: Value,
    },
}

/// A run's bounds.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Heap bytes.
    pub heap: usize,
    /// Stack bytes.
    pub stack: usize,
    /// Wall-clock time.
    pub wall: Duration,
    /// Operations called.
    pub calls: usize,
    /// Bytes of JSON an operation's answer or the program's may be.
    pub result: usize,
    /// Bytes of program text.
    pub program: usize,
    /// Calls under way at once (more wait their turn).
    pub parallel: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self { heap: 64 << 20, stack: 512 << 10, wall: Duration::from_secs(60), calls: 2_000, result: 4 << 20, program: 1 << 20, parallel: 16 }
    }
}

/// Why a run ended without an answer.
#[derive(Debug)]
pub struct CodeError {
    /// The bound or failure (`heap`, `time`, `calls`, `result`, `unsettled`, `program`, `engine`).
    pub code: &'static str,
    /// What happened.
    pub message: String,
    /// The calls made before it ended: `{name, ok}`.
    pub calls: Vec<Value>,
}

fn fail(code: &'static str, message: impl Into<String>, calls: &[Value]) -> CodeError {
    CodeError { code, message: message.into(), calls: calls.to_vec() }
}

const PRELUDE: &str = r#""use strict";
const __kiln_queue = [];
const __kiln_pending = new Map();
let __kiln_id = 0;
let __kiln_state = { settled: false, ok: false, value: undefined };
function __kiln_call(name, input) {
  return new Promise((resolve, reject) => {
    const id = ++__kiln_id;
    __kiln_pending.set(id, { resolve, reject });
    __kiln_queue.push(JSON.stringify({ id, name: String(name), input: input === undefined ? {} : input }));
  });
}
function __kiln_next() { return __kiln_queue.length ? __kiln_queue.shift() : null; }
function __kiln_settle(id, ok, text) {
  const p = __kiln_pending.get(id);
  if (!p) return;
  __kiln_pending.delete(id);
  const v = JSON.parse(text);
  if (ok) { p.resolve(v); } else {
    const e = new Error(v && v.error ? v.error : "the call failed");
    e.result = v;
    p.reject(e);
  }
}
function __kiln_main(promise) {
  promise.then(
    (v) => { __kiln_state = { settled: true, ok: true, value: v }; },
    (e) => { __kiln_state = { settled: true, ok: false, value: e && e.message ? (e.stack ? e.message + "\n" + e.stack : e.message) : String(e) }; });
}
function __kiln_done() {
  if (!__kiln_state.settled) return JSON.stringify({ unsettled: true });
  if (!__kiln_state.ok) return JSON.stringify({ error: __kiln_state.value });
  const v = __kiln_state.value;
  return JSON.stringify({ result: v === undefined ? null : v });
}
function __kiln_build(names) {
  const root = { call: __kiln_call };
  for (const name of names) {
    const parts = name.split(".");
    let o = root;
    for (let i = 0; i < parts.length - 1; i++) { o[parts[i]] = o[parts[i]] || {}; o = o[parts[i]]; }
    o[parts[parts.length - 1]] = (input) => __kiln_call(name, input);
  }
  return Object.freeze(root);
}
"#;

/// The registry as `code_search` programs see it.
pub fn registry(hub: &crate::hub::Hub) -> Value {
    registry_of(&ops::all(hub))
}

/// `ops` as `code_search` programs see them.
pub fn registry_of(ops: &[ops::Info]) -> Value {
    Value::Array(
        ops.iter()
            .map(|o| {
                json!({
                    "name": o.name, "call": format!("kiln.{}", o.name), "tool": super::tool_name(o.name),
                    "summary": o.summary, "read_only": o.read_only, "destructive": o.destructive, "input": (o.input)(),
                })
            })
            .collect(),
    )
}

/// Runs a program against kiln's operations, as `cx`'s caller. Blocks: call it on the blocking
/// pool.
pub fn run(cx: Cx, mode: Mode, program: &str, limits: Limits) -> Result<Value, CodeError> {
    run_on(Arc::new(Ops(cx)), mode, program, limits)
}

/// Runs a program whose calls go to `host`. Blocks.
pub fn run_on(host: Arc<dyn Host>, mode: Mode, program: &str, limits: Limits) -> Result<Value, CodeError> {
    let mut calls: Vec<Value> = Vec::new();
    if program.len() > limits.program {
        return Err(fail("program", format!("the program is {} bytes; the most is {}", program.len(), limits.program), &calls));
    }
    let rt = Runtime::new().map_err(|e| fail("engine", format!("no engine: {e}"), &[]))?;
    rt.set_memory_limit(limits.heap);
    rt.set_max_stack_size(limits.stack);
    let deadline = Instant::now() + limits.wall;
    let timed_out = Arc::new(AtomicBool::new(false));
    let flag = timed_out.clone();
    rt.set_interrupt_handler(Some(Box::new(move || {
        if Instant::now() > deadline {
            flag.store(true, Ordering::Relaxed);
            return true;
        }
        false
    })));
    let ctx = Context::full(&rt).map_err(|e| fail("heap", format!("no memory for a context: {e}"), &[]))?;
    let source = match &mode {
        Mode::Search => {
            let skills: Vec<Value> = super::skills::list().into_iter().map(|s| json!({"name": s.get("name"), "description": s.get("description")})).collect();
            format!(
                "{PRELUDE}\nconst registry = {};\nconst skills = {};\n__kiln_main((async () => {{\n{program}\n}})());",
                host.registry(),
                Value::Array(skills)
            )
        }
        Mode::Run { input } => format!(
            "{PRELUDE}\nconst input = {};\nconst kiln = __kiln_build({});\n__kiln_main((async () => {{\n{program}\n}})());",
            input,
            json!(host.names())
        ),
    };
    let ended = |calls: &[Value], what: &str| {
        if timed_out.load(Ordering::Relaxed) {
            fail("time", format!("the program ran past its {} s", limits.wall.as_secs()), calls)
        } else {
            fail("program", what.to_owned(), calls)
        }
    };
    ctx.with(|ctx| {
        ctx.eval::<(), _>(source).map_err(|_| {
            let caught = ctx.catch();
            match caught.as_exception() {
                Some(ex) => format!("{}\n{}", ex.message().unwrap_or_default(), ex.stack().unwrap_or_default()),
                None => "the program doesn't parse".to_owned(),
            }
        })
    })
    .map_err(|e| ended(&calls, &e))?;
    // Calls end on their own threads and say so here: (the promise's id, the operation, its answer).
    let (ended_tx, ended_rx) = std::sync::mpsc::channel::<(u64, String, Result<Value, Value>)>();
    let mut started = 0usize;
    let mut flying = 0usize;
    let outcome: Result<(), CodeError> = 'drive: loop {
        loop {
            match rt.execute_pending_job() {
                Ok(true) => {}
                Ok(false) => break,
                Err(_) => break 'drive Err(ended(&calls, "a promise job threw")),
            }
        }
        // Start what the program asked for, as many at once as it may.
        while flying < limits.parallel {
            let next = match ctx.with(|ctx| ctx.eval::<Option<String>, _>("__kiln_next()")) {
                Ok(n) => n,
                Err(_) => break 'drive Err(ended(&calls, "the call queue failed")),
            };
            let Some(next) = next else { break };
            if started >= limits.calls {
                break 'drive Err(fail("calls", format!("the program made more than {} calls", limits.calls), &calls));
            }
            let call: Value = match serde_json::from_str(&next) {
                Ok(c) => c,
                Err(e) => break 'drive Err(fail("program", format!("a call didn't encode: {e}"), &calls)),
            };
            let id = call.get("id").and_then(Value::as_u64).unwrap_or(0);
            let name = call.get("name").and_then(Value::as_str).unwrap_or("").to_owned();
            let input = call.get("input").cloned().unwrap_or(Value::Null);
            let (h, tx) = (Arc::clone(&host), ended_tx.clone());
            let spawned = std::thread::Builder::new().name("kiln-code-call".into()).spawn(move || {
                let answer = h.call(&name, input);
                let _ = tx.send((id, name, answer));
            });
            if let Err(e) = spawned {
                break 'drive Err(fail("engine", format!("the call could not run: {e}"), &calls));
            }
            started += 1;
            flying += 1;
        }
        if flying == 0 {
            break Ok(());
        }
        // The next call to end, within the program's time.
        let left = deadline.saturating_duration_since(Instant::now());
        let Ok((id, name, answer)) = ended_rx.recv_timeout(left) else {
            break 'drive Err(fail("time", format!("the program ran past its {} s", limits.wall.as_secs()), &calls));
        };
        flying -= 1;
        let (mut ok, payload) = match answer {
            Ok(v) => (true, v),
            Err(e) => (false, e),
        };
        calls.push(json!({"name": name, "ok": ok}));
        let mut text = serde_json::to_string(&payload).unwrap_or_else(|_| "null".into());
        if text.len() > limits.result {
            ok = false;
            text = json!({"error": format!("{name}'s answer is {} bytes, more than the {} a program may take; filter or page on the server", text.len(), limits.result)}).to_string();
        }
        let settle = format!("__kiln_settle({id}, {ok}, {})", serde_json::to_string(&text).unwrap_or_else(|_| "\"null\"".into()));
        if ctx.with(|ctx| ctx.eval::<(), _>(settle)).is_err() {
            break 'drive Err(ended(&calls, "settling a call failed"));
        }
        if timed_out.load(Ordering::Relaxed) {
            break 'drive Err(ended(&calls, "time"));
        }
    };
    if flying > 0 {
        host.abandon();
    }
    outcome?;
    let done: String = ctx.with(|ctx| ctx.eval::<String, _>("__kiln_done()")).map_err(|_| ended(&calls, "the answer could not be read"))?;
    if done.len() > limits.result {
        return Err(fail("result", format!("the program's answer is {} bytes; the most is {}", done.len(), limits.result), &calls));
    }
    let v: Value = serde_json::from_str(&done).map_err(|e| fail("program", format!("the answer isn't JSON: {e}"), &calls))?;
    if v.get("unsettled").is_some() {
        return Err(fail("unsettled", "the program awaited something that never settled", &calls));
    }
    if let Some(e) = v.get("error") {
        return Err(fail("program", e.as_str().unwrap_or("the program threw").to_owned(), &calls));
    }
    Ok(json!({"result": v.get("result").cloned().unwrap_or(Value::Null), "calls": calls}))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Calls that take `ms` milliseconds each and answer with their input; `fail` refuses.
    struct Slow {
        ms: u64,
        abandoned: AtomicBool,
    }

    impl Host for Slow {
        fn names(&self) -> Vec<String> {
            vec!["machines.run".into(), "fail".into()]
        }

        fn registry(&self) -> Value {
            json!([{"name": "machines.run"}, {"name": "fail"}])
        }

        fn call(&self, name: &str, input: Value) -> Result<Value, Value> {
            std::thread::sleep(Duration::from_millis(self.ms));
            if name == "fail" { Err(json!({"error": "no such machine"})) } else { Ok(json!({"echo": input})) }
        }

        fn abandon(&self) {
            self.abandoned.store(true, Ordering::Relaxed);
        }
    }

    fn slow(ms: u64) -> Arc<Slow> {
        Arc::new(Slow { ms, abandoned: AtomicBool::new(false) })
    }

    #[test]
    fn calls_a_program_starts_together_run_side_by_side() -> Result<(), String> {
        let host = slow(400);
        let started = Instant::now();
        let program = "const r = await Promise.all([1, 2, 3, 4].map(n => kiln.machines.run({n}))); return r.map(x => x.echo.n);";
        let out = run_on(host, Mode::Run { input: Value::Null }, program, Limits::default()).map_err(|e| e.message)?;
        assert_eq!(out.get("result"), Some(&json!([1, 2, 3, 4])));
        assert_eq!(out.get("calls").and_then(Value::as_array).map(Vec::len), Some(4));
        // Four calls of 400 ms each, at once: well under the 1.6 s they'd take one after another.
        assert!(started.elapsed() < Duration::from_millis(1_200), "{:?}", started.elapsed());
        Ok(())
    }

    #[test]
    fn a_refused_call_throws_with_its_answer_and_a_program_can_catch_it() -> Result<(), String> {
        let program = "try { await kiln.fail({}); return 'no'; } catch (e) { return {message: e.message, result: e.result}; }";
        let out = run_on(slow(1), Mode::Run { input: Value::Null }, program, Limits::default()).map_err(|e| e.message)?;
        assert_eq!(out.pointer("/result/message"), Some(&json!("no such machine")));
        assert_eq!(out.pointer("/result/result/error"), Some(&json!("no such machine")));
        Ok(())
    }

    #[test]
    fn a_program_past_its_time_ends_and_what_it_left_running_is_stopped() {
        let host = slow(2_000);
        let limits = Limits { wall: Duration::from_millis(300), ..Limits::default() };
        let started = Instant::now();
        let e = run_on(host.clone(), Mode::Run { input: Value::Null }, "return await kiln.machines.run({});", limits).err();
        assert_eq!(e.as_ref().map(|e| e.code), Some("time"));
        assert!(started.elapsed() < Duration::from_millis(1_500), "it doesn't wait for the call");
        assert!(host.abandoned.load(Ordering::Relaxed), "the host is told to stop what the call started");
    }

    #[test]
    fn at_most_parallel_calls_run_at_once() -> Result<(), String> {
        let host = slow(200);
        let limits = Limits { parallel: 2, ..Limits::default() };
        let started = Instant::now();
        let out = run_on(host, Mode::Run { input: Value::Null }, "return (await Promise.all([1, 2, 3, 4].map(n => kiln.machines.run({n})))).length;", limits).map_err(|e| e.message)?;
        assert_eq!(out.get("result"), Some(&json!(4)));
        // Two at a time: two rounds of 200 ms.
        assert!(started.elapsed() >= Duration::from_millis(390), "{:?}", started.elapsed());
        Ok(())
    }

    #[test]
    fn a_search_sees_the_hosts_registry() -> Result<(), String> {
        let out = run_on(slow(1), Mode::Search, "return registry.map(o => o.name);", Limits::default()).map_err(|e| e.message)?;
        assert_eq!(out.get("result"), Some(&json!(["machines.run", "fail"])));
        Ok(())
    }
}
