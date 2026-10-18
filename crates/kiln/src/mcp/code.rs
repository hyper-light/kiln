//! Code mode's sandbox: QuickJS-NG (rquickjs) inside kiln, with hard bounds.
//!
//! A program is the body of an async function. Operations are async functions on `kiln`
//! (generated from the registry: `kiln.cd.apps.list`, `kiln.ci.runs.list`) and on `kiln.call(name, input)`. A call
//! queues a promise; the driver here runs the engine's pending jobs, takes the next queued call,
//! runs the operation, settles the promise and goes on, so the engine never calls back into
//! kiln and an operation never runs on the engine's stack. Only what the program returns leaves
//! the sandbox.
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
}

impl Default for Limits {
    fn default() -> Self {
        Self { heap: 64 << 20, stack: 512 << 10, wall: Duration::from_secs(60), calls: 2_000, result: 4 << 20, program: 1 << 20 }
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
    Value::Array(
        ops::all(hub)
            .iter()
            .map(|o| {
                json!({
                    "name": o.name, "call": format!("kiln.{}", o.name), "tool": super::tool_name(o.name),
                    "summary": o.summary, "read_only": o.read_only, "destructive": o.destructive, "input": (o.input)(),
                })
            })
            .collect(),
    )
}

/// Runs a program. Blocks: call it on the blocking pool.
pub fn run(cx: Cx, mode: Mode, program: &str, limits: Limits) -> Result<Value, CodeError> {
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
                registry(&cx.hub),
                Value::Array(skills)
            )
        }
        Mode::Run { input } => {
            let names: Vec<&str> = ops::all(&cx.hub).iter().map(|o| o.name).collect();
            format!(
                "{PRELUDE}\nconst input = {};\nconst kiln = __kiln_build({});\n__kiln_main((async () => {{\n{program}\n}})());",
                input,
                json!(names)
            )
        }
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
    loop {
        loop {
            match rt.execute_pending_job() {
                Ok(true) => {}
                Ok(false) => break,
                Err(_) => return Err(ended(&calls, "a promise job threw")),
            }
        }
        let next: Option<String> = ctx.with(|ctx| ctx.eval::<Option<String>, _>("__kiln_next()")).map_err(|_| ended(&calls, "the call queue failed"))?;
        let Some(next) = next else { break };
        if calls.len() >= limits.calls {
            return Err(fail("calls", format!("the program made more than {} calls", limits.calls), &calls));
        }
        let call: Value = serde_json::from_str(&next).map_err(|e| fail("program", format!("a call didn't encode: {e}"), &calls))?;
        let id = call.get("id").and_then(Value::as_u64).unwrap_or(0);
        let name = call.get("name").and_then(Value::as_str).unwrap_or("").to_owned();
        let input = call.get("input").cloned().unwrap_or(Value::Null);
        let cx2 = cx.clone();
        let n2 = name.clone();
        let answer = crate::rt::block_on(async move { Ok(ops::call(cx2, &n2, input).await) })
            .map_err(|e| fail("engine", format!("the call could not run: {e:#}"), &calls))?;
        let (ok, payload) = match answer {
            Ok(v) => (true, v),
            Err((status, msg)) => (false, json!({"error": msg, "status": status.as_u16()})),
        };
        calls.push(json!({"name": name, "ok": ok}));
        let mut text = serde_json::to_string(&payload).unwrap_or_else(|_| "null".into());
        let mut ok = ok;
        if text.len() > limits.result {
            ok = false;
            text = json!({"error": format!("{name}'s answer is {} bytes, more than the {} a program may take; filter or page on the server", text.len(), limits.result)}).to_string();
        }
        let settle = format!("__kiln_settle({id}, {ok}, {})", serde_json::to_string(&text).unwrap_or_else(|_| "\"null\"".into()));
        ctx.with(|ctx| ctx.eval::<(), _>(settle)).map_err(|_| ended(&calls, "settling a call failed"))?;
        if timed_out.load(Ordering::Relaxed) {
            return Err(ended(&calls, "time"));
        }
    }
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
