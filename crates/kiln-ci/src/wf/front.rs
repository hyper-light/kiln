//! YAML → the workflow model, for both dialects (docs/ci.md W02-R05, W02-R06): GitHub's rules
//! strictly in the GHA dialect, kiln's keys and v1's forms in the kiln dialect. Structural
//! problems become diagnostics; whatever can be read is still read, so the editor sees as much as
//! possible.

use super::diag::{Diag, Range, suggest};
use super::ir::{
    Axis, Concurrency, Container, Defaults, Environment, Input, Job, JobKiln, JobSecrets, Matrix, MatrixSpec, Need, Output, Permissions, RunsOn, Schedule,
    SecretDecl, Step, StepKiln, Strategy, Tpl, Trigger, Workflow, YVal,
};
use super::keys::{self, Key};
use super::yaml::{Kind, Node, Scalar};
use crate::expr::Dialect;

/// Converts with the rules of one dialect, collecting diagnostics.
pub(crate) struct Fe<'a> {
    pub dialect: Dialect,
    /// Overlay mode: only overlay keys and `runs-on` (W02-R07).
    pub overlay: bool,
    pub diags: &'a mut Vec<Diag>,
}

pub(crate) fn is_job_id(s: &str) -> bool {
    let mut c = s.chars();
    matches!(c.next(), Some(f) if f.is_ascii_alphabetic() || f == '_') && c.all(|x| x.is_ascii_alphanumeric() || x == '-' || x == '_')
}

impl Fe<'_> {
    fn kiln(&self) -> bool {
        self.dialect == Dialect::Kiln
    }

    fn err(&mut self, code: &'static str, msg: impl Into<String>, range: Range, path: &str) {
        self.diags.push(Diag::error(code, msg, range).at(path));
    }

    /// A mapping's entries whose keys this dialect allows here; reports the others.
    pub fn entries<'n>(&mut self, node: &'n Node, table: &[Key], path: &str) -> Vec<(String, &'n Node, Range)> {
        let Some(map) = node.as_map() else {
            self.err("KCI1011", format!("`{path}` must be a mapping, not {}", node.type_name()), node.range, path);
            return Vec::new();
        };
        let mut out = Vec::new();
        for (k, v) in map {
            let Some(name) = k.text() else {
                self.err("KCI1011", "keys must be strings", k.range, path);
                continue;
            };
            let at = if path.is_empty() { name.clone() } else { format!("{path}.{name}") };
            match keys::find(table, &name) {
                Some(key) if key.kiln && !self.kiln() && !(self.overlay && key.overlay) => {
                    let msg = if key.overlay {
                        format!("`{name}` is a kiln key: set it in an overlay (.kiln/overlays/) so the workflow keeps working on GitHub")
                    } else {
                        format!("`{name}` is a kiln key, not valid in a GitHub workflow")
                    };
                    self.err("KCI5002", msg, k.range, &at);
                }
                Some(key) if self.overlay && !key.overlay && !(name == "runs-on" || name == "jobs" || name == "steps") => {
                    self.err("KCI1010", format!("`{name}` can't be set by an overlay: overlays add kiln keys and may replace `runs-on`, nothing else"), k.range, &at);
                }
                Some(_) => out.push((name, v, k.range)),
                None => {
                    let candidates = table.iter().filter(|c| self.kiln() || !c.kiln).map(|c| c.name);
                    let hint = suggest(&name, candidates).map(|s| format!(" (did you mean `{s}`?)")).unwrap_or_default();
                    self.err("KCI1010", format!("unknown key `{name}`{hint}"), k.range, &at);
                }
            }
        }
        out
    }

    pub fn str(&mut self, n: &Node, path: &str) -> Option<String> {
        match n.text() {
            Some(t) => Some(t),
            None => {
                self.err("KCI1011", format!("`{path}` must be a string, not {}", n.type_name()), n.range, path);
                None
            }
        }
    }

    pub fn tpl(&mut self, n: &Node, path: &str) -> Option<Tpl> {
        self.str(n, path).map(|s| Tpl::new(s, n.range))
    }

    fn bool(&mut self, n: &Node, path: &str) -> Option<bool> {
        match n.as_scalar() {
            Some(Scalar::Bool(b)) => Some(*b),
            _ => {
                self.err("KCI1011", format!("`{path}` must be true or false, not {}", n.type_name()), n.range, path);
                None
            }
        }
    }

    /// A boolean, or a string holding an expression.
    fn bool_or_expr(&mut self, n: &Node, path: &str) -> Option<YVal> {
        match n.as_scalar() {
            Some(Scalar::Bool(b)) => Some(YVal::Bool(*b)),
            Some(Scalar::Str(s)) if s.contains("${{") => Some(YVal::Str(Tpl::new(s.clone(), n.range))),
            _ => {
                self.err("KCI1011", format!("`{path}` must be a boolean or an expression"), n.range, path);
                None
            }
        }
    }

    fn num_or_expr(&mut self, n: &Node, path: &str) -> Option<YVal> {
        match n.as_scalar() {
            #[allow(clippy::cast_precision_loss)]
            Some(Scalar::Int(i)) => Some(YVal::Num(*i as f64)),
            Some(Scalar::Float(f)) => Some(YVal::Num(*f)),
            Some(Scalar::Str(s)) if s.contains("${{") => Some(YVal::Str(Tpl::new(s.clone(), n.range))),
            _ => {
                self.err("KCI1011", format!("`{path}` must be a number or an expression"), n.range, path);
                None
            }
        }
    }

    /// A string or a sequence of strings.
    pub fn str_list(&mut self, n: &Node, path: &str) -> Vec<String> {
        match &n.kind {
            Kind::Seq(items) => items.iter().enumerate().filter_map(|(i, x)| self.str(x, &format!("{path}[{i}]"))).collect(),
            Kind::Scalar(Scalar::Null, _) => Vec::new(),
            Kind::Scalar(..) => n.text().into_iter().collect(),
            Kind::Map(_) => {
                self.err("KCI1011", format!("`{path}` must be a string or a list of strings"), n.range, path);
                Vec::new()
            }
        }
    }

    fn tpl_list(&mut self, n: &Node, path: &str) -> Vec<Tpl> {
        match &n.kind {
            Kind::Seq(items) => items.iter().enumerate().filter_map(|(i, x)| self.tpl(x, &format!("{path}[{i}]"))).collect(),
            Kind::Scalar(Scalar::Null, _) => Vec::new(),
            _ => self.tpl(n, path).into_iter().collect(),
        }
    }

    pub fn yval(&mut self, n: &Node) -> YVal {
        match &n.kind {
            Kind::Scalar(Scalar::Null, _) => YVal::Null,
            Kind::Scalar(Scalar::Bool(b), _) => YVal::Bool(*b),
            #[allow(clippy::cast_precision_loss)]
            Kind::Scalar(Scalar::Int(i), _) => YVal::Num(*i as f64),
            Kind::Scalar(Scalar::Float(f), _) => YVal::Num(*f),
            Kind::Scalar(Scalar::Str(s), _) => YVal::Str(Tpl::new(s.clone(), n.range)),
            Kind::Seq(items) => YVal::Seq(items.iter().map(|x| self.yval(x)).collect()),
            Kind::Map(items) => YVal::Map(items.iter().map(|(k, v)| (k.text().unwrap_or_default(), self.yval(v))).collect()),
        }
    }

    /// `env:`-style mappings: names to strings (numbers and booleans as text).
    pub fn str_map(&mut self, n: &Node, path: &str) -> Vec<(String, Tpl)> {
        let Some(map) = n.as_map() else {
            if n.is_null() {
                return Vec::new();
            }
            if n.text().is_some_and(|t| t.contains("${{")) {
                // `env: ${{ fromJSON(…) }}` is allowed by GitHub in some places; keep it whole.
                return vec![(String::new(), Tpl::new(n.text().unwrap_or_default(), n.range))];
            }
            self.err("KCI1011", format!("`{path}` must be a mapping of names to values"), n.range, path);
            return Vec::new();
        };
        let mut out = Vec::new();
        for (k, v) in map {
            let name = k.text().unwrap_or_default();
            match v.kind {
                Kind::Scalar(..) => out.push((name, Tpl::new(v.text().unwrap_or_default(), v.range))),
                _ => self.err("KCI1011", format!("`{path}.{name}` must be a string, number or boolean"), v.range, &format!("{path}.{name}")),
            }
        }
        out
    }

    // ------------------------------------------------------------------ workflow

    pub fn workflow(&mut self, root: &Node, path_name: &str) -> Workflow {
        let mut wf = Workflow { dialect: Some(self.dialect), path: path_name.to_owned(), checkout: self.kiln(), ..Workflow::default() };
        let entries = self.entries(root, keys::WORKFLOW, "");
        let mut saw_on = false;
        let mut saw_jobs = false;
        for (k, v, kr) in entries {
            match k.as_str() {
                "name" => wf.name = self.str(v, "name"),
                "run-name" => wf.run_name = self.tpl(v, "run-name"),
                "on" => {
                    saw_on = true;
                    wf.on = self.on(v);
                }
                "permissions" => wf.permissions = self.permissions(v, "permissions"),
                "env" => wf.env = self.str_map(v, "env"),
                "defaults" => wf.defaults = self.defaults(v, "defaults"),
                "concurrency" => wf.concurrency = self.concurrency(v, "concurrency"),
                "cache-mode" => wf.cache_mode = self.cache_mode(v, "cache-mode"),
                "jobs" => {
                    saw_jobs = true;
                    wf.jobs = self.jobs(v);
                }
                "secrets" => wf.secrets = self.secret_decls(v, "secrets"),
                "supersede" => wf.supersede = Some(self.yval(v)),
                "routing" => wf.routing_infer = v.get("infer").and_then(Node::as_scalar) == Some(&Scalar::Bool(true)),
                "run-env" => wf.run_env = self.bool(v, "run-env").unwrap_or(false),
                "kiln" => wf.honour_skip_ci = v.get("honour-skip-ci").and_then(Node::as_scalar) == Some(&Scalar::Bool(true)),
                "checkout" => wf.checkout = self.bool(v, "checkout").unwrap_or(true),
                _ => {
                    let _ = kr;
                }
            }
        }
        if !self.overlay {
            if !saw_on && !self.kiln() {
                self.err("KCI1014", "a workflow needs `on:`", root.range, "on");
            }
            if !saw_jobs {
                self.err("KCI1014", "a workflow needs `jobs:`", root.range, "jobs");
            }
        }
        // v1: manual runs are allowed unless `on.manual: false` (W02-R06).
        if self.kiln() && !self.overlay && !wf.on.iter().any(|t| t.event == "workflow_dispatch") && !root.get("on").and_then(|o| o.get("manual")).is_some_and(|m| m.as_scalar() == Some(&Scalar::Bool(false))) {
            wf.on.push(Trigger { event: "workflow_dispatch".into(), ..Trigger::default() });
        }
        wf
    }

    fn secret_decls(&mut self, n: &Node, path: &str) -> Vec<SecretDecl> {
        let Some(map) = n.as_map() else {
            if !n.is_null() {
                self.err("KCI1011", format!("`{path}` must be a mapping of secret names"), n.range, path);
            }
            return Vec::new();
        };
        let mut out = Vec::new();
        for (k, v) in map {
            let name = k.text().unwrap_or_default();
            let mut d = SecretDecl { name: name.clone(), ..SecretDecl::default() };
            if !v.is_null() {
                for (key, val, _) in self.entries(v, keys::SECRET_DECL, &format!("{path}.{name}")) {
                    match key.as_str() {
                        "description" => d.description = val.text(),
                        "required" => d.required = self.bool(val, &format!("{path}.{name}.required")).unwrap_or(false),
                        _ => {}
                    }
                }
            }
            out.push(d);
        }
        out
    }

    fn defaults(&mut self, n: &Node, path: &str) -> Defaults {
        let mut d = Defaults::default();
        for (k, v, _) in self.entries(n, keys::DEFAULTS, path) {
            if k == "run" {
                for (rk, rv, _) in self.entries(v, keys::DEFAULTS_RUN, &format!("{path}.run")) {
                    match rk.as_str() {
                        "shell" => d.shell = self.tpl(rv, &format!("{path}.run.shell")),
                        "working-directory" => d.working_directory = self.tpl(rv, &format!("{path}.run.working-directory")),
                        _ => {}
                    }
                }
            }
        }
        d
    }

    fn concurrency(&mut self, n: &Node, path: &str) -> Option<Concurrency> {
        if n.as_scalar().is_some() {
            return self.tpl(n, path).map(|group| Concurrency { group, ..Concurrency::default() });
        }
        let mut c = Concurrency::default();
        let mut has_group = false;
        for (k, v, _) in self.entries(n, keys::CONCURRENCY, path) {
            let at = format!("{path}.{k}");
            match k.as_str() {
                "group" => {
                    has_group = true;
                    c.group = self.tpl(v, &at).unwrap_or_default();
                }
                "cancel-in-progress" => c.cancel_in_progress = self.bool_or_expr(v, &at),
                "limit" => c.limit = v.text().and_then(|t| t.parse().ok()),
                "queue" => {
                    let q = self.str(v, &at);
                    if q.as_deref().is_some_and(|q| !matches!(q, "single" | "max")) {
                        self.err("KCI1015", "`queue` is `single` or `max`", v.range, &at);
                    }
                    c.queue = q;
                }
                _ => {}
            }
        }
        if !has_group {
            self.err("KCI1014", "`concurrency` needs a `group`", n.range, path);
        }
        if c.queue.as_deref() == Some("max") && c.cancel_in_progress == Some(YVal::Bool(true)) {
            self.err("KCI1015", "`queue: max` can't be combined with `cancel-in-progress: true`", n.range, path);
        }
        Some(c)
    }

    fn cache_mode(&mut self, n: &Node, path: &str) -> Option<String> {
        let m = self.str(n, path)?;
        if !matches!(m.as_str(), "read" | "write" | "write-only" | "none") {
            self.err("KCI1015", "`cache-mode` is `read`, `write`, `write-only` or `none`", n.range, path);
        }
        Some(m)
    }

    fn permissions(&mut self, n: &Node, path: &str) -> Option<Permissions> {
        if let Some(t) = n.as_scalar().and_then(|_| n.text()) {
            return match t.as_str() {
                "read-all" => Some(Permissions::ReadAll),
                "write-all" => Some(Permissions::WriteAll),
                _ => {
                    self.err("KCI1015", "`permissions` is `read-all`, `write-all` or a mapping of scopes", n.range, path);
                    None
                }
            };
        }
        let Some(map) = n.as_map() else {
            self.err("KCI1011", "`permissions` is `read-all`, `write-all` or a mapping of scopes", n.range, path);
            return None;
        };
        let mut scopes = Vec::new();
        for (k, v) in map {
            let scope = k.text().unwrap_or_default();
            let at = format!("{path}.{scope}");
            let known = keys::PERMISSION_SCOPES.contains(&scope.as_str()) || (self.kiln() && keys::KILN_PERMISSION_SCOPES.contains(&scope.as_str()));
            if !known {
                let hint = suggest(&scope, keys::PERMISSION_SCOPES.iter().copied()).map(|s| format!(" (did you mean `{s}`?)")).unwrap_or_default();
                self.err("KCI1015", format!("unknown permission scope `{scope}`{hint}"), k.range, &at);
                continue;
            }
            let level = v.text().unwrap_or_default();
            let ok = matches!(level.as_str(), "read" | "write" | "none") || (keys::KILN_PERMISSION_SCOPES.contains(&scope.as_str()) && !level.is_empty());
            if !ok {
                self.err("KCI1015", format!("`{scope}` is `read`, `write` or `none`"), v.range, &at);
                continue;
            }
            scopes.push((scope, level));
        }
        Some(Permissions::Scopes(scopes))
    }

    // ------------------------------------------------------------------ on

    fn on(&mut self, n: &Node) -> Vec<Trigger> {
        let mut out = Vec::new();
        match &n.kind {
            Kind::Scalar(..) => {
                if let Some(e) = n.text() {
                    self.event_name(&e, n.range, "on");
                    out.push(Trigger { event: e, range: n.range, ..Trigger::default() });
                }
            }
            Kind::Seq(items) => {
                for (i, x) in items.iter().enumerate() {
                    if let Some(e) = self.str(x, &format!("on[{i}]")) {
                        self.event_name(&e, x.range, "on");
                        out.push(Trigger { event: e, range: x.range, ..Trigger::default() });
                    }
                }
            }
            Kind::Map(map) => {
                for (k, v) in map {
                    let Some(e) = k.text() else { continue };
                    if !self.event_name(&e, k.range, "on") {
                        continue;
                    }
                    let path = format!("on.{e}");
                    // v1 forms (kiln dialect).
                    if self.kiln() && e == "manual" {
                        match v.as_scalar() {
                            Some(Scalar::Bool(true)) => out.push(Trigger { event: "workflow_dispatch".into(), range: k.range, ..Trigger::default() }),
                            Some(Scalar::Bool(false)) => {}
                            _ => {
                                let mut t = self.trigger("workflow_dispatch", v, &path);
                                t.range = k.range;
                                out.push(t);
                            }
                        }
                        continue;
                    }
                    if self.kiln() && e == "tag" {
                        let mut t = Trigger { event: "push".into(), range: k.range, ..Trigger::default() };
                        if let Some(b) = v.get("branches") {
                            t.tags = Some(self.str_list(b, &format!("{path}.branches")));
                        } else {
                            t.tags = Some(vec!["*".into()]);
                        }
                        out.push(t);
                        continue;
                    }
                    if e == "schedule" {
                        let mut t = Trigger { event: e.clone(), range: k.range, ..Trigger::default() };
                        t.schedules = self.schedules(v, &path);
                        out.push(t);
                        continue;
                    }
                    let mut t = self.trigger(&e, v, &path);
                    t.range = k.range;
                    out.push(t);
                }
            }
        }
        out
    }

    /// Whether `e` is an event this dialect knows; reports it otherwise.
    fn event_name(&mut self, e: &str, range: Range, path: &str) -> bool {
        if keys::GHA_EVENTS.contains(&e) {
            return true;
        }
        if keys::KILN_EVENTS.contains(&e) {
            if self.kiln() || self.overlay {
                return true;
            }
            self.err("KCI5002", format!("`{e}` is a kiln event: use it in .kiln/ workflows, or add it to this workflow with an overlay"), range, path);
            return false;
        }
        let all = keys::GHA_EVENTS.iter().chain(keys::KILN_EVENTS.iter().filter(|_| self.kiln())).copied();
        let hint = suggest(e, all).map(|s| format!(" (did you mean `{s}`?)")).unwrap_or_default();
        self.err("KCI1018", format!("unknown event `{e}`{hint}"), range, path);
        false
    }

    fn trigger(&mut self, event: &str, v: &Node, path: &str) -> Trigger {
        let mut t = Trigger { event: event.to_owned(), ..Trigger::default() };
        if v.is_null() {
            return t;
        }
        let allowed = keys::event_keys(event);
        for (k, val, kr) in self.entries(v, keys::FILTERS, path) {
            let at = format!("{path}.{k}");
            if !allowed.contains(&k.as_str()) {
                self.err("KCI1010", format!("`{event}` doesn't take `{k}` (it takes {})", allowed.iter().map(|a| format!("`{a}`")).collect::<Vec<_>>().join(", ")), kr, &at);
                continue;
            }
            match k.as_str() {
                "types" => {
                    let ts = self.str_list(val, &at);
                    if let Some(known) = keys::event_types(event) {
                        for ty in &ts {
                            if !known.contains(&ty.as_str()) {
                                let hint = suggest(ty, known.iter().copied()).map(|s| format!(" (did you mean `{s}`?)")).unwrap_or_default();
                                self.err("KCI1015", format!("`{event}` has no activity type `{ty}`{hint}"), val.range, &at);
                            }
                        }
                    }
                    t.types = ts;
                }
                "branches" => t.branches = Some(self.str_list(val, &at)),
                "branches-ignore" => t.branches_ignore = Some(self.str_list(val, &at)),
                "tags" => t.tags = Some(self.str_list(val, &at)),
                "tags-ignore" => t.tags_ignore = Some(self.str_list(val, &at)),
                "paths" => t.paths = Some(self.str_list(val, &at)),
                "paths-ignore" => t.paths_ignore = Some(self.str_list(val, &at)),
                "workflows" => t.workflows = self.str_list(val, &at),
                "inputs" => t.inputs = self.inputs(event, val, &at),
                "outputs" => {
                    if let Some(map) = val.as_map() {
                        for (ok, ov) in map {
                            let name = ok.text().unwrap_or_default();
                            let mut o = super::ir::CallOutput { name: name.clone(), ..super::ir::CallOutput::default() };
                            let mut has_value = false;
                            for (key, x, _) in self.entries(ov, keys::CALL_OUTPUT, &format!("{at}.{name}")) {
                                match key.as_str() {
                                    "description" => o.description = x.text(),
                                    "value" => {
                                        has_value = true;
                                        o.value = self.tpl(x, &format!("{at}.{name}.value")).unwrap_or_default();
                                    }
                                    _ => {}
                                }
                            }
                            if !has_value {
                                self.err("KCI1014", format!("output `{name}` needs a `value`"), ok.range, &format!("{at}.{name}"));
                            }
                            t.outputs.push(o);
                        }
                    }
                }
                "secrets" => t.secrets = self.secret_decls(val, &at),
                other => t.extra.push((other.to_owned(), self.yval(val))),
            }
        }
        // GitHub: branches and branches-ignore (or tags and tags-ignore, paths and paths-ignore)
        // can't both filter one event.
        for (a, b) in [("branches", "branches-ignore"), ("tags", "tags-ignore"), ("paths", "paths-ignore")] {
            if v.get(a).is_some() && v.get(b).is_some() {
                self.err("KCI1015", format!("`{a}` and `{b}` can't both be used for `{event}`"), v.range, path);
            }
        }
        if event == "workflow_run" && t.workflows.is_empty() {
            self.err("KCI1014", "`workflow_run` needs `workflows`", v.range, path);
        }
        t
    }

    fn inputs(&mut self, event: &str, n: &Node, path: &str) -> Vec<Input> {
        let Some(map) = n.as_map() else {
            if !n.is_null() {
                self.err("KCI1011", format!("`{path}` must be a mapping of inputs"), n.range, path);
            }
            return Vec::new();
        };
        let call = event == "workflow_call";
        let table = if call { keys::CALL_INPUT } else { keys::DISPATCH_INPUT };
        let mut out = Vec::new();
        for (k, v) in map {
            let name = k.text().unwrap_or_default();
            let at = format!("{path}.{name}");
            let mut i = Input { name: name.clone(), ..Input::default() };
            if !v.is_null() {
                for (key, x, _) in self.entries(v, table, &at) {
                    match key.as_str() {
                        "description" => i.description = x.text(),
                        "required" => i.required = self.bool(x, &format!("{at}.required")).unwrap_or(false),
                        "default" => i.default = Some(self.yval(x)),
                        "type" => i.ty = x.text(),
                        "options" => i.options = self.str_list(x, &format!("{at}.options")),
                        _ => {}
                    }
                }
            }
            let types: &[&str] = if call { &["boolean", "number", "string"] } else { &["string", "boolean", "choice", "number", "environment"] };
            if let Some(ty) = &i.ty
                && !types.contains(&ty.as_str())
            {
                self.err("KCI1015", format!("input type is one of {}", types.join(", ")), v.range, &format!("{at}.type"));
            }
            if call && i.ty.is_none() {
                self.err("KCI1014", format!("`workflow_call` input `{name}` needs a `type`"), k.range, &at);
            }
            if i.ty.as_deref() == Some("choice") && i.options.is_empty() {
                self.err("KCI1014", format!("choice input `{name}` needs `options`"), k.range, &at);
            }
            out.push(i);
        }
        out
    }

    fn schedules(&mut self, n: &Node, path: &str) -> Vec<Schedule> {
        let Some(items) = n.as_seq() else {
            self.err("KCI1011", "`schedule` is a list of `- cron: '…'`", n.range, path);
            return Vec::new();
        };
        let mut out = Vec::new();
        for (i, x) in items.iter().enumerate() {
            let at = format!("{path}[{i}]");
            // v1: a plain cron string.
            if self.kiln()
                && let Some(c) = x.as_scalar().and_then(|_| x.text())
            {
                self.check_cron(&c, x.range, &at);
                out.push(Schedule { cron: c, range: x.range, ..Schedule::default() });
                continue;
            }
            let Some(map) = x.as_map() else {
                self.err("KCI1011", "each schedule is `cron: '…'`", x.range, &at);
                continue;
            };
            let mut s = Schedule { range: x.range, ..Schedule::default() };
            let mut cron_range = x.range;
            for (k, v) in map {
                let key = k.text().unwrap_or_default();
                let kiln_key = matches!(key.as_str(), "timezone" | "branch" | "inputs" | "jitter" | "skip-if-unchanged" | "catch-up");
                if kiln_key && !(self.kiln() || self.overlay) {
                    self.err("KCI5002", format!("`{key}` on a schedule is a kiln key: set it in an overlay"), k.range, &format!("{at}.{key}"));
                    continue;
                }
                match key.as_str() {
                    "cron" => {
                        s.cron = v.text().unwrap_or_default();
                        cron_range = v.range;
                    }
                    "timezone" => s.timezone = v.text(),
                    "branch" => s.branch = v.text(),
                    "inputs" => s.inputs = v.as_map().map(|m| m.iter().map(|(a, b)| (a.text().unwrap_or_default(), self.yval(b))).collect()).unwrap_or_default(),
                    "jitter" => s.jitter = v.text(),
                    "skip-if-unchanged" => s.skip_if_unchanged = v.as_scalar() == Some(&Scalar::Bool(true)),
                    "catch-up" => s.catch_up = v.as_scalar() == Some(&Scalar::Bool(true)),
                    other => self.err("KCI1010", format!("unknown schedule key `{other}`"), k.range, &format!("{at}.{other}")),
                }
            }
            if s.cron.is_empty() {
                self.err("KCI1014", "a schedule needs `cron`", x.range, &at);
            } else {
                self.check_cron(&s.cron.clone(), cron_range, &at);
            }
            out.push(s);
        }
        out
    }

    fn check_cron(&mut self, c: &str, range: Range, path: &str) {
        if c.split_whitespace().count() != 5 {
            self.err("KCI1019", format!("`{c}` is not a cron expression of five fields (minute hour day month weekday)"), range, path);
        }
    }

    // ------------------------------------------------------------------ jobs

    fn jobs(&mut self, n: &Node) -> Vec<Job> {
        let Some(map) = n.as_map() else {
            self.err("KCI1011", "`jobs` must be a mapping of job ids to jobs", n.range, "jobs");
            return Vec::new();
        };
        if map.is_empty() && !self.overlay {
            self.err("KCI1014", "`jobs` needs at least one job", n.range, "jobs");
        }
        let mut out = Vec::new();
        for (k, v) in map {
            let id = k.text().unwrap_or_default();
            if !is_job_id(&id) {
                self.err("KCI1013", format!("job id `{id}` must start with a letter or `_` and hold only letters, digits, `-` and `_`"), k.range, &format!("jobs.{id}"));
            }
            out.push(self.job(&id, v, k.range));
        }
        out
    }

    /// Only the kiln keys of a job mapping (overlays).
    pub fn job_kiln_only(&mut self, id: &str, n: &Node) -> JobKiln {
        self.job(id, n, n.range).kiln
    }

    fn job(&mut self, id: &str, n: &Node, range: Range) -> Job {
        let path = format!("jobs.{id}");
        let mut j = Job { id: id.to_owned(), range, ..Job::default() };
        let entries = self.entries(n, keys::JOB, &path);
        let mut kiln = JobKiln::default();
        let mut v1_matrix: Option<&Node> = None;
        for (k, v, kr) in &entries {
            let at = format!("{path}.{k}");
            match k.as_str() {
                "name" => j.name = self.tpl(v, &at),
                "permissions" => j.permissions = self.permissions(v, &at),
                "needs" => j.needs = self.needs(v, &at),
                "if" => j.cond = self.tpl(v, &at),
                "runs-on" => j.runs_on = self.runs_on(v, &at),
                "environment" => {
                    j.environment = if v.as_scalar().is_some() {
                        self.tpl(v, &at).map(|name| Environment { name, url: None, deployment: None })
                    } else {
                        let mut e = Environment::default();
                        for (ek, ev, _) in self.entries(v, keys::ENVIRONMENT, &at) {
                            match ek.as_str() {
                                "name" => e.name = self.tpl(ev, &format!("{at}.name")).unwrap_or_default(),
                                "url" => e.url = self.tpl(ev, &format!("{at}.url")),
                                "deployment" => e.deployment = self.bool_or_expr(ev, &format!("{at}.deployment")),
                                _ => {}
                            }
                        }
                        Some(e)
                    }
                }
                "concurrency" => j.concurrency = self.concurrency(v, &at),
                "outputs" => j.outputs = self.outputs(v, &at),
                "env" => j.env = self.str_map(v, &at),
                "defaults" => j.defaults = self.defaults(v, &at),
                "steps" => j.steps = self.steps(v, &at),
                "timeout-minutes" => j.timeout_minutes = self.num_or_expr(v, &at),
                "strategy" => j.strategy = Some(self.strategy(v, &at)),
                "continue-on-error" => j.continue_on_error = self.bool_or_expr(v, &at),
                "container" => j.container = self.container(v, &at),
                "services" => {
                    if let Some(map) = v.as_map() {
                        for (sk, sv) in map {
                            let sid = sk.text().unwrap_or_default();
                            if let Some(c) = self.container(sv, &format!("{at}.{sid}")) {
                                j.services.push((sid, c));
                            }
                        }
                    } else if !v.is_null() {
                        self.err("KCI1011", "`services` must be a mapping of service ids to containers", v.range, &at);
                    }
                }
                "uses" => {
                    j.uses = self.str(v, &at);
                    if let Some(u) = &j.uses
                        && !reusable_ref_ok(u)
                    {
                        self.err("KCI4005", format!("`{u}` is not a reusable workflow: use `./.github/workflows/<file>.yml` or `owner/repo/.github/workflows/<file>.yml@ref`"), v.range, &at);
                    }
                }
                "with" => j.with = v.as_map().map(|m| m.iter().map(|(a, b)| (a.text().unwrap_or_default(), self.yval(b))).collect()).unwrap_or_default(),
                "secrets" => {
                    j.secrets = match v.text().as_deref() {
                        Some("inherit") => Some(JobSecrets::Inherit),
                        _ => Some(JobSecrets::Map(self.str_map(v, &at))),
                    }
                }
                "snapshot" => j.snapshot = Some(self.yval(v)),
                "cache-mode" => j.cache_mode = self.cache_mode(v, &at),
                // kiln
                "matrix" => v1_matrix = Some(v),
                "toolchain" => kiln.toolchain = self.str(v, &at),
                "image" => kiln.image = self.tpl(v, &at),
                "exclusive" => kiln.exclusive = self.bool(v, &at).unwrap_or(false),
                "resources" => kiln.resources = Some(self.yval(v)),
                "requires" => kiln.requires = self.tpl_list(v, &at),
                "prefer" => kiln.prefer = v.as_seq().map(|s| s.iter().map(|x| self.yval(x)).collect()).unwrap_or_default(),
                "priority" => kiln.priority = Some(self.yval(v)),
                "retry" => kiln.retry = Some(self.yval(v)),
                "retries" => {
                    let n = v.text().unwrap_or_default();
                    kiln.retry = Some(YVal::Map(vec![
                        ("max".into(), YVal::Num(n.parse().unwrap_or(0.0))),
                        ("on".into(), YVal::Seq(vec![YVal::Str(Tpl::new("runner-lost", v.range))])),
                    ]));
                }
                "reuse" => kiln.reuse = Some(self.yval(v)),
                "cache" => kiln.cache = v.as_seq().map(|s| s.iter().map(|x| self.yval(x)).collect()).unwrap_or_default(),
                "exports" => kiln.exports_env = v.get("env").map(|e| self.str_list(e, &format!("{at}.env"))).unwrap_or_default(),
                "env-from" => kiln.env_from = self.str_list(v, &at),
                "integrations" => kiln.integrations = self.str_list(v, &at),
                "registries" => kiln.registries = self.str_list(v, &at),
                "network" => kiln.network = Some(self.yval(v)),
                "debug" => kiln.debug = Some(self.yval(v)),
                "attest" => kiln.attest = Some(self.yval(v)),
                "quarantine" => kiln.quarantine_enforce = v.text().as_deref() == Some("enforce"),
                "bench-gate" => kiln.bench_gate = Some(self.yval(v)),
                "deploy" => kiln.deploy = Some(self.yval(v)),
                "agents" => kiln.agents = Some(self.yval(v)),
                "artifacts" => kiln.artifacts = self.str_list(v, &at),
                "checkout" => kiln.checkout = self.bool(v, &at),
                _ => {
                    let _ = kr;
                }
            }
        }
        if let Some(m) = v1_matrix {
            let at = format!("{path}.matrix");
            if j.strategy.as_ref().is_some_and(|s| s.matrix.is_some()) {
                self.err("KCI1015", "both `matrix` and `strategy.matrix`: use one", m.range, &at);
            } else {
                let matrix = self.matrix(m, &at);
                j.strategy.get_or_insert_with(Strategy::default).matrix = matrix;
            }
        }
        j.kiln = kiln;
        if !self.overlay {
            self.job_shape(&j, n, &entries, &path);
        }
        j
    }

    /// What every job must have, and what jobs calling a reusable workflow must not.
    fn job_shape(&mut self, j: &Job, n: &Node, entries: &[(String, &Node, Range)], path: &str) {
        if j.uses.is_some() {
            const CALL_KEYS: &[&str] = &["name", "uses", "with", "secrets", "strategy", "needs", "if", "permissions", "concurrency"];
            for (k, _, kr) in entries {
                if !CALL_KEYS.contains(&k.as_str()) && !(self.kiln() && keys::find(keys::JOB, k).is_some_and(|x| x.kiln)) {
                    self.err("KCI1016", format!("a job that calls a reusable workflow can't have `{k}`"), *kr, &format!("{path}.{k}"));
                }
            }
            return;
        }
        if j.runs_on.is_none() && !self.kiln() {
            self.err("KCI1014", "a job needs `runs-on` (or `uses` to call a reusable workflow)", n.range, &format!("{path}.runs-on"));
        }
        if !entries.iter().any(|(k, _, _)| k == "steps") {
            self.err("KCI1014", "a job needs `steps` (or `uses` to call a reusable workflow)", n.range, &format!("{path}.steps"));
        } else if j.steps.is_empty() {
            self.err("KCI1014", "a job needs at least one step", n.range, &format!("{path}.steps"));
        }
    }

    fn needs(&mut self, n: &Node, path: &str) -> Vec<Need> {
        let items: Vec<&Node> = match &n.kind {
            Kind::Seq(items) => items.iter().collect(),
            Kind::Scalar(Scalar::Null, _) => Vec::new(),
            _ => vec![n],
        };
        let mut out = Vec::new();
        for (i, x) in items.into_iter().enumerate() {
            let at = format!("{path}[{i}]");
            if let Some(m) = x.as_map() {
                if !self.kiln() && !self.overlay {
                    self.err("KCI5002", "leg-level needs (`{ job, matrix: same }`) are kiln's: use an overlay", x.range, &at);
                    continue;
                }
                let job = m.iter().find(|(k, _)| k.text().as_deref() == Some("job")).and_then(|(_, v)| v.text()).unwrap_or_default();
                let same = m.iter().any(|(k, v)| k.text().as_deref() == Some("matrix") && v.text().as_deref() == Some("same"));
                let filter = m.iter().find(|(k, _)| k.text().as_deref() == Some("where")).and_then(|(_, v)| v.text().map(|t| Tpl::new(t, v.range)));
                if job.is_empty() {
                    self.err("KCI1014", "a leg-level need names its `job`", x.range, &at);
                }
                out.push(Need::Legs { job, matrix_same: same, filter });
            } else if let Some(s) = self.str(x, &at) {
                out.push(Need::Job(s));
            }
        }
        out
    }

    fn runs_on(&mut self, n: &Node, path: &str) -> Option<RunsOn> {
        match &n.kind {
            Kind::Map(_) => {
                let mut group = None;
                let mut labels = Vec::new();
                if let Some(map) = n.as_map() {
                    for (k, v) in map {
                        match k.text().as_deref() {
                            Some("group") => group = self.tpl(v, &format!("{path}.group")),
                            Some("labels") => labels = self.tpl_list(v, &format!("{path}.labels")),
                            other => self.err("KCI1010", format!("`runs-on` takes `group` and `labels`, not `{}`", other.unwrap_or_default()), k.range, path),
                        }
                    }
                }
                Some(RunsOn::Group { group, labels })
            }
            _ => Some(RunsOn::Labels(self.tpl_list(n, path))),
        }
    }

    fn outputs(&mut self, n: &Node, path: &str) -> Vec<Output> {
        let Some(map) = n.as_map() else {
            if !n.is_null() {
                self.err("KCI1011", "`outputs` must be a mapping of names to values", n.range, path);
            }
            return Vec::new();
        };
        let mut out = Vec::new();
        for (k, v) in map {
            let name = k.text().unwrap_or_default();
            let at = format!("{path}.{name}");
            if let Some(m) = v.as_map() {
                if !self.kiln() {
                    self.err("KCI5002", "`{ value, secret }` outputs are kiln's", v.range, &at);
                    continue;
                }
                let value = m.iter().find(|(a, _)| a.text().as_deref() == Some("value")).and_then(|(_, b)| b.text().map(|t| Tpl::new(t, b.range))).unwrap_or_default();
                let secret = m.iter().any(|(a, b)| a.text().as_deref() == Some("secret") && b.as_scalar() == Some(&Scalar::Bool(true)));
                out.push(Output { name, value, secret });
            } else if let Some(value) = self.tpl(v, &at) {
                out.push(Output { name, value, secret: false });
            }
        }
        out
    }

    fn container(&mut self, n: &Node, path: &str) -> Option<Container> {
        if n.as_scalar().is_some() {
            return self.tpl(n, path).map(|image| Container { image, ..Container::default() });
        }
        let mut c = Container::default();
        let mut has_image = false;
        for (k, v, _) in self.entries(n, keys::CONTAINER, path) {
            let at = format!("{path}.{k}");
            match k.as_str() {
                "image" => {
                    has_image = true;
                    c.image = self.tpl(v, &at).unwrap_or_default();
                }
                "credentials" => {
                    let user = v.get("username").and_then(|u| u.text().map(|t| Tpl::new(t, u.range))).unwrap_or_default();
                    let pass = v.get("password").and_then(|u| u.text().map(|t| Tpl::new(t, u.range))).unwrap_or_default();
                    let _ = self.entries(v, keys::CREDENTIALS, &at);
                    c.credentials = Some((user, pass));
                }
                "env" => c.env = self.str_map(v, &at),
                "ports" => c.ports = v.as_seq().map(|s| s.iter().map(|x| self.yval(x)).collect()).unwrap_or_default(),
                "volumes" => c.volumes = self.tpl_list(v, &at),
                "options" => c.options = self.tpl(v, &at),
                _ => {}
            }
        }
        if !has_image {
            self.err("KCI1014", "a container needs an `image`", n.range, path);
        }
        Some(c)
    }

    fn strategy(&mut self, n: &Node, path: &str) -> Strategy {
        let mut s = Strategy::default();
        for (k, v, _) in self.entries(n, keys::STRATEGY, path) {
            let at = format!("{path}.{k}");
            match k.as_str() {
                "matrix" => s.matrix = self.matrix(v, &at),
                "fail-fast" => s.fail_fast = if self.kiln() && v.as_map().is_some() { Some(self.yval(v)) } else { self.bool_or_expr(v, &at) },
                "max-parallel" => s.max_parallel = if self.kiln() && v.as_map().is_some() { Some(self.yval(v)) } else { self.num_or_expr(v, &at) },
                "when" => s.when = self.tpl(v, &at),
                "shard" => s.shard = Some(self.yval(v)),
                "chunk" => s.chunk = Some(self.yval(v)),
                _ => {}
            }
        }
        s
    }

    fn matrix(&mut self, n: &Node, path: &str) -> Option<Matrix> {
        if let Some(t) = n.as_scalar().and_then(|_| n.text()) {
            if t.contains("${{") {
                return Some(Matrix::Expr(Tpl::new(t, n.range)));
            }
            self.err("KCI1011", "`matrix` is a mapping of axes, or an expression", n.range, path);
            return None;
        }
        let Some(map) = n.as_map() else {
            self.err("KCI1011", "`matrix` is a mapping of axes, or an expression", n.range, path);
            return None;
        };
        let mut m = MatrixSpec::default();
        for (k, v) in map {
            let name = k.text().unwrap_or_default();
            let at = format!("{path}.{name}");
            let reserved = keys::find(keys::MATRIX_RESERVED, &name);
            match (name.as_str(), reserved) {
                ("include", _) => m.include = Some(self.yval(v)),
                ("exclude", _) => m.exclude = Some(self.yval(v)),
                // In GitHub workflows kiln's reserved names are ordinary axes (`platforms` is a
                // common one).
                (_, Some(r)) if r.kiln && !(self.kiln() || self.overlay) => {
                    if let Some(a) = self.axis(v, &at) {
                        m.axes.push((name, a));
                    }
                }
                ("where", _) => m.filter = self.tpl(v, &at),
                ("platforms", _) => m.platforms = self.axis(v, &at),
                ("emulate", _) => m.emulate = self.str_list(v, &at),
                _ => {
                    if let Some(a) = self.axis(v, &at) {
                        m.axes.push((name, a));
                    }
                }
            }
        }
        Some(Matrix::Spec(m))
    }

    fn axis(&mut self, v: &Node, at: &str) -> Option<Axis> {
        match &v.kind {
            Kind::Seq(items) => {
                if items.is_empty() {
                    self.err("KCI1017", format!("matrix axis `{at}` has no values"), v.range, at);
                }
                Some(Axis::Values(items.iter().map(|x| self.yval(x)).collect()))
            }
            Kind::Scalar(Scalar::Str(s), _) if s.contains("${{") => Some(Axis::Expr(Tpl::new(s.clone(), v.range))),
            _ => {
                self.err("KCI1011", format!("matrix axis `{at}` must be a list of values, or an expression"), v.range, at);
                None
            }
        }
    }

    // ------------------------------------------------------------------ steps

    pub fn steps(&mut self, n: &Node, path: &str) -> Vec<Step> {
        let Some(items) = n.as_seq() else {
            self.err("KCI1011", format!("`{path}` must be a list of steps"), n.range, path);
            return Vec::new();
        };
        let mut out: Vec<Step> = Vec::new();
        for (i, x) in items.iter().enumerate() {
            let at = format!("{path}[{i}]");
            let s = self.step(x, &at);
            if let Some(id) = &s.id
                && out.iter().any(|o| o.id.as_ref() == Some(id))
            {
                self.err("KCI4003", format!("two steps have the id `{id}`"), x.range, &at);
            }
            out.push(s);
        }
        out
    }

    pub fn step(&mut self, n: &Node, path: &str) -> Step {
        let mut s = Step { range: n.range, ..Step::default() };
        let mut kiln = StepKiln::default();
        for (k, v, _) in self.entries(n, keys::STEP, path) {
            let at = format!("{path}.{k}");
            match k.as_str() {
                "id" => {
                    s.id = self.str(v, &at);
                    if let Some(id) = &s.id
                        && !is_job_id(id)
                    {
                        self.err("KCI1013", format!("step id `{id}` must start with a letter or `_` and hold only letters, digits, `-` and `_`"), v.range, &at);
                    }
                }
                "if" => s.cond = self.tpl(v, &at),
                "name" => s.name = self.tpl(v, &at),
                "uses" => {
                    s.uses = self.str(v, &at);
                    if let Some(u) = &s.uses {
                        self.check_uses(u, v.range, &at);
                    }
                }
                "run" => s.run = self.tpl(v, &at),
                "shell" => s.shell = self.tpl(v, &at),
                "with" => s.with = v.as_map().map(|m| m.iter().map(|(a, b)| (a.text().unwrap_or_default(), self.yval(b))).collect()).unwrap_or_default(),
                "env" => {
                    s.env = self.str_map(v, &at);
                    s.env_range = Some(v.range);
                }
                "continue-on-error" => s.continue_on_error = self.bool_or_expr(v, &at),
                "timeout-minutes" => s.timeout_minutes = self.num_or_expr(v, &at),
                "working-directory" => s.working_directory = self.tpl(v, &at),
                "background" => s.background = self.bool_or_expr(v, &at),
                "wait" => s.wait = self.str_list(v, &at),
                "wait-all" => s.wait_all = !matches!(v.as_scalar(), Some(Scalar::Bool(false))),
                "cancel" => {
                    // GitHub: one background step, by id.
                    if v.as_seq().is_some() {
                        self.err("KCI1011", "`cancel` names one background step: a step id, not a list", v.range, &at);
                    }
                    s.cancel = self.str_list(v, &at);
                }
                "parallel" => s.parallel = self.steps(v, &at),
                "retry" => kiln.retry = Some(self.yval(v)),
                "with-secrets" => kiln.with_secrets = self.str_map(v, &at).into_iter().map(|(a, b)| (a, b.src)).collect(),
                "test-results" => kiln.test_results = Some(self.yval(v)),
                "integrations" => kiln.integrations = self.str_list(v, &at),
                "tty" => kiln.tty = self.bool(v, &at).unwrap_or(false),
                _ => {}
            }
        }
        s.kiln = kiln;
        if !self.overlay {
            match (&s.run, &s.uses, s.is_control()) {
                (Some(_), Some(_), _) => self.err("KCI1012", "a step has `run` or `uses`, not both", n.range, path),
                (None, None, false) => self.err("KCI1012", "a step needs `run` or `uses` (or `wait`, `wait-all`, `cancel` or `parallel`)", n.range, path),
                (Some(_) | None, Some(_) | None, true) if s.run.is_some() || s.uses.is_some() => {
                    self.err("KCI1012", "a `wait`, `wait-all`, `cancel` or `parallel` step runs nothing itself: it can't have `run` or `uses`", n.range, path)
                }
                _ => {}
            }
            // GitHub: "A `wait` step always runs and does not support the `if` conditional" (and
            // so do `wait-all` and `cancel`).
            if s.cond.is_some() && (!s.wait.is_empty() || s.wait_all || !s.cancel.is_empty()) {
                self.err("KCI1012", "a `wait`, `wait-all` or `cancel` step always runs: it can't have `if`", n.range, path);
            }
        }
        s
    }

    fn check_uses(&mut self, u: &str, range: Range, path: &str) {
        // `./path` (the workspace), `$/path` (this repository at the running commit, no checkout
        // needed), `docker://image`.
        if u.starts_with("./") || u.starts_with("$/") || u.starts_with("docker://") {
            return;
        }
        if self.kiln() && (u.starts_with("kiln://") || (u.starts_with("kiln/") && u.contains('@'))) {
            return;
        }
        if u.starts_with("kiln/") && u.contains('@') {
            // kiln's built-in actions resolve locally from GHA workflows too (W07-R10).
            return;
        }
        match u.split_once('@') {
            Some((repo, r)) if !r.is_empty() && repo.split('/').filter(|p| !p.is_empty()).count() >= 2 => {
                if u.contains("${{") {
                    self.err("KCI4006", "`uses` can't hold expressions", range, path);
                }
            }
            _ => self.err("KCI4006", format!("`{u}` is not an action: use `owner/repo@ref`, `owner/repo/path@ref`, `./path` or `docker://image`"), range, path),
        }
    }
}

fn reusable_ref_ok(u: &str) -> bool {
    let wf = |p: &str| p.contains("/.github/workflows/") || p.starts_with(".github/workflows/");
    if let Some(own) = u.strip_prefix("$/") {
        return own.starts_with(".github/workflows/") && !u.contains('@');
    }
    if let Some(local) = u.strip_prefix("./") {
        return local.starts_with(".github/workflows/") && !u.contains('@') || local.starts_with("templates/") || local.starts_with(".kiln/");
    }
    if u.starts_with("kiln://") {
        return u.contains('@');
    }
    match u.split_once('@') {
        Some((path, r)) => !r.is_empty() && wf(path) && path.split('/').count() >= 5,
        None => false,
    }
}
