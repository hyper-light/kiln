//! What work needs of a runner, in one grammar (docs/runners.md): what placement reserves on the
//! runner it picks (CPUs, memory, disk, GPUs), constraints on that runner's facts and labels
//! (routing's language, W05-R22), and whether the work takes the runner whole. `kiln run --needs`,
//! MCP's run and CI legs (a job's `resources:`, `requires:` and `exclusive:`) each make a [`Needs`].
//!
//! Its spelling is words separated by commas or spaces, `cpus=8,mem=32g,linux/amd64,+gpu,tool:cargo,exclusive`:
//!
//! - `cpus=8` (or `cpu=`; `0.5` and `500m` too), `mem=32g` (or `memory=`), `disk=50g`, `gpus=1` (or
//!   `gpu=`): reserved on the runner. Sizes are binary whatever their case (`32g`, `32G`, `32GB`,
//!   `32Gi`), as `kiln run --on` takes them; a plain number is bytes.
//! - `linux/amd64`, `linux`, `arm64`, …: the runner's OS and architecture, by any of their names.
//! - `+gpu` (or `label=gpu`): the runner carries the label.
//! - `tool:cargo`, `executor:docker`, `flag:avx2`, `binfmt:linux/arm64`: it has that tool, executor,
//!   CPU flag or binfmt handler.
//! - `cpus>=16`, `mem>=32g`: that many CPUs in all, that much memory free, as `kiln run --on` takes
//!   them: checked, not reserved.
//! - `fact==value`, `fact!=value`, `fact>=n`, `fact<=n`, `fact>n`, `fact<n`: any fact routing knows
//!   (`disk.free>=50Gi`, `os.distro==fedora`, `custom.rack==a3`).
//! - `exclusive`: the runner whole, with nothing else beside the work.
//!
//! Anything else is an error that names the word: `memm=4g: no such need`.

use serde::{Deserialize, Serialize};

use super::pools::{self, ResourceSpec};
use super::routing::{self, Constraint};
use super::state::Resources;
use crate::expr;
use crate::wf::ir::{JobKiln, YVal};

/// What work needs of a runner.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Needs {
    /// What placement reserves on the runner it picks.
    pub request: Resources,
    /// Constraints on the runner's facts and labels, in routing's language, as written.
    pub requires: Vec<String>,
    /// The runner whole: nothing else runs beside the work.
    pub exclusive: bool,
}

/// Reads the spelling (the module's docs). An error names the first word that isn't a need.
pub fn parse(spec: &str) -> Result<Needs, String> {
    let mut needs = Needs::default();
    for word in spec.split([',', ' ']).map(str::trim).filter(|w| !w.is_empty()) {
        needs.add(word).map_err(|why| format!("{word}: {why}"))?;
    }
    Ok(needs)
}

/// What a job's kiln keys ask (Appendix E): its `resources:` reserved, its `requires:` as written,
/// and `exclusive:`.
pub fn of_job(k: &JobKiln) -> Result<Needs, String> {
    let request = match &k.resources {
        Some(v) => resource_spec(v).resources()?,
        None => Resources::default(),
    };
    Ok(Needs { request, requires: k.requires.iter().map(|t| t.src.clone()).collect(), exclusive: k.exclusive })
}

/// A job's `resources:` (`{ cpu, memory, disk, gpu }`) as written.
pub fn resource_spec(v: &YVal) -> ResourceSpec {
    let text = |k: &str| match v.get(k) {
        Some(YVal::Str(t)) => Some(t.src.clone()),
        Some(YVal::Num(n)) => Some(n.to_string()),
        _ => None,
    };
    ResourceSpec { cpu: text("cpu"), memory: text("memory"), disk: text("disk"), gpu: text("gpu").and_then(|g| g.trim().parse().ok()) }
}

impl Needs {
    /// Its constraints parsed, each with its text, as routing's requirement holds them.
    pub fn constraints(&self) -> Result<Vec<(String, Constraint)>, String> {
        self.requires.iter().map(|s| Constraint::parse(s).map(|c| (s.clone(), c))).collect()
    }

    /// In words, for placement's messages: "8 CPUs, 32 GiB of memory, os == linux, the runner whole".
    pub fn describe(&self) -> String {
        let mut parts = request_words(&self.request);
        parts.extend(self.requires.iter().cloned());
        if self.exclusive {
            parts.push("the runner whole".into());
        }
        if parts.is_empty() { "nothing in particular".into() } else { parts.join(", ") }
    }

    /// Adds one word of the spelling. An error says why it isn't one, without the word.
    pub fn add(&mut self, w: &str) -> Result<(), String> {
        if w.eq_ignore_ascii_case("exclusive") {
            self.exclusive = true;
            return Ok(());
        }
        // `cpus>=16` is a fact checked and `cpus=8` a request: comparisons first.
        if let Some((fact, op, value)) = comparison(w) {
            return self.compare(fact, op, value);
        }
        if let Some((key, value)) = w.split_once('=') {
            return self.ask(key, value);
        }
        if let Some(label) = w.strip_prefix('+') {
            return self.require(format!("label {}", quote(label)?));
        }
        // Before `/`: `binfmt:linux/arm64` is one.
        if let Some((kind, item)) = w.split_once(':') {
            let fact = match kind.to_ascii_lowercase().as_str() {
                "tool" => "tools",
                "executor" => "executors",
                "flag" => "cpu.flags",
                "binfmt" => "binfmt",
                _ => return Err("no such need (tool:, executor:, flag: or binfmt:)".into()),
            };
            return self.require(format!("{fact} has {}", quote(item)?));
        }
        if let Some((os, arch)) = w.split_once('/') {
            let os = os_name(os).ok_or("no such OS (linux, macos, windows, freebsd)")?;
            let arch = arch_name(arch).ok_or("no such architecture (amd64, arm64, x86, arm, riscv64)")?;
            self.require(format!("os == {os}"))?;
            return self.require(format!("arch == {arch}"));
        }
        if let Some(os) = os_name(w) {
            return self.require(format!("os == {os}"));
        }
        if let Some(arch) = arch_name(w) {
            return self.require(format!("arch == {arch}"));
        }
        Err("no such need".into())
    }

    /// `cpus=8`, `mem=32g`, `disk=50g`, `gpus=1`, `label=gpu`.
    fn ask(&mut self, key: &str, value: &str) -> Result<(), String> {
        match key.to_ascii_lowercase().as_str() {
            "cpus" | "cpu" => self.request.cpu = pools::cpus(value).ok_or("not a number of CPUs (8, 0.5, 500m)")?,
            "mem" | "memory" => self.request.mem = size(value).ok_or("not a size (32g, 512m)")?,
            "disk" => self.request.disk = size(value).ok_or("not a size (50g, 512m)")?,
            "gpus" | "gpu" => self.request.gpu = value.trim().parse().map_err(|_| "not a number of GPUs")?,
            "label" => return self.require(format!("label {}", quote(value)?)),
            _ => return Err("no such need".into()),
        }
        Ok(())
    }

    /// `cpus>=16` and `mem>=32g` as `kiln run --on` takes them (CPUs in all, memory free), or any
    /// fact routing knows.
    fn compare(&mut self, fact: &str, op: &str, value: &str) -> Result<(), String> {
        let (fact, value) = match fact.to_ascii_lowercase().as_str() {
            "cpus" | "cpu" => ("cpu.threads".to_owned(), value.to_owned()),
            "mem" | "memory" => ("mem.free".to_owned(), binary(size(value).ok_or("not a size (32g, 512m)")?)),
            f if routing::known_fact(f) => (f.to_owned(), value.to_owned()),
            _ => return Err("no such fact (os, arch, cpu.cores, mem.total, disk.free, tools, …, custom.<name>)".into()),
        };
        self.require(format!("{fact} {op} {}", quote(&value)?))
    }

    /// Adds a constraint once, as routing reads it.
    fn require(&mut self, src: String) -> Result<(), String> {
        Constraint::parse(&src)?;
        if !self.requires.contains(&src) {
            self.requires.push(src);
        }
        Ok(())
    }
}

/// `fact>=n` and the like: the fact, its operator and the value.
fn comparison(w: &str) -> Option<(&str, &'static str, &str)> {
    ["==", "!=", ">=", "<=", ">", "<"].into_iter().find_map(|op| w.split_once(op).map(|(fact, value)| (fact, op, value)))
}

/// A value as routing's language reads it: as it is when it's one word, else quoted.
fn quote(v: &str) -> Result<String, String> {
    if v.is_empty() {
        Err("a value is missing".into())
    } else if !v.contains(|c: char| "()[],!<>=&|'\"".contains(c)) {
        Ok(v.to_owned())
    } else if !v.contains('\'') {
        Ok(format!("'{v}'"))
    } else if !v.contains('"') {
        Ok(format!("\"{v}\""))
    } else {
        Err("a value can't hold both quotes".into())
    }
}

/// Bytes as the spelling writes them: `32g`, `32G`, `32GB`, `32Gi` and `512m` are binary, and a
/// plain number is bytes.
pub fn size(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let (num, unit) = s.split_at(s.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(s.len()));
    let k: u64 = match unit {
        "" | "b" => 1,
        "k" | "kb" | "ki" | "kib" => 1 << 10,
        "m" | "mb" | "mi" | "mib" => 1 << 20,
        "g" | "gb" | "gi" | "gib" => 1 << 30,
        "t" | "tb" | "ti" | "tib" => 1 << 40,
        _ => return None,
    };
    let v = num.parse::<f64>().ok()? * k as f64;
    (v.is_finite() && v >= 0.0).then_some(v as u64)
}

/// Bytes as routing's language reads them back, in the largest binary unit that holds them whole:
/// `32Gi`, `1536Mi`, or plain bytes.
fn binary(n: u64) -> String {
    [(1u64 << 40, "Ti"), (1 << 30, "Gi"), (1 << 20, "Mi"), (1 << 10, "Ki")]
        .iter()
        .find(|(k, _)| n >= *k && n.is_multiple_of(*k))
        .map_or_else(|| n.to_string(), |(k, u)| format!("{}{u}", n / k))
}

/// What a request reserves, in words, leaving out what it doesn't ask for: `8 CPUs`, `32 GiB of
/// memory`, `50 GiB of disk`, `1 GPU`.
pub fn request_words(r: &Resources) -> Vec<String> {
    let mut parts = Vec::new();
    if r.cpu > 0.0 {
        parts.push(cpus(r.cpu));
    }
    if r.mem > 0 {
        parts.push(format!("{} of memory", show_size(r.mem)));
    }
    if r.disk > 0 {
        parts.push(format!("{} of disk", show_size(r.disk)));
    }
    if r.gpu > 0 {
        parts.push(gpus(r.gpu));
    }
    parts
}

/// `8 CPUs`, `1 CPU`, `0.5 CPUs`.
pub fn cpus(n: f64) -> String {
    format!("{} CPU{}", expr::js_number_to_string(n), if (n - 1.0).abs() < f64::EPSILON { "" } else { "s" })
}

/// `1 GPU`, `2 GPUs`, `no GPU`.
pub fn gpus(n: u32) -> String {
    match n {
        0 => "no GPU".into(),
        1 => "1 GPU".into(),
        n => format!("{n} GPUs"),
    }
}

/// `32 GiB`, `512 MiB`, `1.5 TiB`.
pub fn show_size(n: u64) -> String {
    let units = [(1u64 << 40, "TiB"), (1 << 30, "GiB"), (1 << 20, "MiB"), (1 << 10, "KiB")];
    units
        .iter()
        .find(|(k, _)| n >= *k)
        .map_or_else(|| format!("{n} bytes"), |(k, u)| format!("{} {u}", expr::js_number_to_string((n as f64 / *k as f64 * 10.0).round() / 10.0)))
}

/// The names people and tools use for an OS, as Rust names it.
pub fn os_name(s: &str) -> Option<&'static str> {
    Some(match s.to_ascii_lowercase().as_str() {
        "linux" | "gnu" => "linux",
        "macos" | "mac" | "darwin" | "osx" | "apple" => "macos",
        "windows" | "win" | "win32" | "win64" => "windows",
        "freebsd" => "freebsd",
        _ => return None,
    })
}

/// The names people and tools use for an architecture, as Rust names it.
pub fn arch_name(s: &str) -> Option<&'static str> {
    Some(match s.to_ascii_lowercase().as_str() {
        "x86_64" | "amd64" | "x64" | "x86-64" | "intel" => "x86_64",
        "aarch64" | "arm64" | "armv8" | "apple-silicon" => "aarch64",
        "x86" | "i386" | "i686" => "x86",
        "arm" | "armv7" | "armhf" => "arm",
        "riscv64" => "riscv64",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expr::Dialect;
    use crate::wf::parse_workflow;

    const GIB: u64 = 1 << 30;

    fn needs(request: Resources, requires: &[&str], exclusive: bool) -> Needs {
        Needs { request, requires: requires.iter().map(|s| (*s).to_owned()).collect(), exclusive }
    }

    #[test]
    fn the_spelling_reads_requests_constraints_and_exclusive() -> Result<(), String> {
        let cases: &[(&str, Needs)] = &[
            (
                "cpus=8,mem=32g,linux/amd64,+gpu,tool:cargo,exclusive",
                needs(Resources { cpu: 8.0, mem: 32 * GIB, ..Resources::default() }, &["os == linux", "arch == x86_64", "label gpu", "tools has cargo"], true),
            ),
            ("cpu=500m memory=512Mi disk=50GB gpus=2", needs(Resources { cpu: 0.5, mem: 512 << 20, disk: 50 * GIB, gpu: 2 }, &[], false)),
            ("macos, arm64", needs(Resources::default(), &["os == macos", "arch == aarch64"], false)),
            ("darwin/apple-silicon", needs(Resources::default(), &["os == macos", "arch == aarch64"], false)),
            ("cpus>=16,mem>=32g", needs(Resources::default(), &["cpu.threads >= 16", "mem.free >= 32Gi"], false)),
            ("mem>=1.5g,mem>=1000", needs(Resources::default(), &["mem.free >= 1536Mi", "mem.free >= 1000"], false)),
            ("disk.free>=50Gi,os.distro==fedora,custom.rack!=a3", needs(Resources::default(), &["disk.free >= 50Gi", "os.distro == fedora", "custom.rack != a3"], false)),
            ("executor:docker,flag:avx2,binfmt:linux/arm64", needs(Resources::default(), &["executors has docker", "cpu.flags has avx2", "binfmt has linux/arm64"], false)),
            ("label=bench,+bench", needs(Resources::default(), &["label bench"], false)),
            ("", Needs::default()),
        ];
        for (spelling, want) in cases {
            assert_eq!(&parse(spelling)?, want, "{spelling}");
            // Every constraint is one routing reads.
            want.constraints()?;
        }
        Ok(())
    }

    #[test]
    fn a_bad_spelling_names_the_bad_part() {
        let cases = [
            ("memm=4g", "memm=4g: no such need"),
            ("cpus=8,mem=lots", "mem=lots: not a size (32g, 512m)"),
            ("cpus=many", "cpus=many: not a number of CPUs (8, 0.5, 500m)"),
            ("gpus=-1", "gpus=-1: not a number of GPUs"),
            ("linx/amd64", "linx/amd64: no such OS (linux, macos, windows, freebsd)"),
            ("linux/sparc", "linux/sparc: no such architecture (amd64, arm64, x86, arm, riscv64)"),
            ("fedora", "fedora: no such need"),
            ("cpu.coress>=8", "cpu.coress>=8: no such fact (os, arch, cpu.cores, mem.total, disk.free, tools, …, custom.<name>)"),
            ("tol:cargo", "tol:cargo: no such need (tool:, executor:, flag: or binfmt:)"),
            ("disk.free>=", "disk.free>=: a value is missing"),
            ("+", "+: a value is missing"),
        ];
        for (spelling, want) in cases {
            assert_eq!(parse(spelling), Err(want.to_owned()), "{spelling}");
        }
    }

    /// Each spelling makes the same needs as its workflow form: a job's `resources:`, `requires:`
    /// and `exclusive:`.
    #[test]
    fn each_spelling_equals_its_workflow_form() -> Result<(), String> {
        let cases = [
            ("cpus=8,mem=32g,linux/amd64,exclusive", "resources: { cpu: 8, memory: 32Gi }\n    requires: ['os == linux', 'arch == x86_64']\n    exclusive: true"),
            ("cpu=500m,disk=50Gi,gpus=1,+gpu", "resources: { cpu: 500m, disk: 50Gi, gpu: 1 }\n    requires: ['label gpu']"),
            ("macos,tool:xcodebuild", "requires: ['os == macos', 'tools has xcodebuild']"),
            ("", "toolchain: rust"),
        ];
        for (spelling, keys) in cases {
            let src = format!("on: push\njobs:\n  a:\n    {keys}\n    steps:\n      - run: make\n");
            let (wf, diags) = parse_workflow(".kiln/ci.yaml", &src, Dialect::Kiln);
            let job = wf.as_ref().and_then(|w| w.job("a")).ok_or_else(|| format!("{keys}: {diags:?}"))?;
            assert_eq!(of_job(&job.kiln)?, parse(spelling)?, "{spelling} against {keys}");
        }
        Ok(())
    }

    #[test]
    fn needs_say_what_they_are() -> Result<(), String> {
        assert_eq!(parse("cpus=8,mem=32g,linux/amd64,exclusive")?.describe(), "8 CPUs, 32 GiB of memory, os == linux, arch == x86_64, the runner whole");
        assert_eq!(parse("cpus=1,disk=1536m,gpus=1")?.describe(), "1 CPU, 1.5 GiB of disk, 1 GPU");
        assert_eq!(Needs::default().describe(), "nothing in particular");
        Ok(())
    }

    #[test]
    fn sizes_are_binary_in_any_case() {
        for (s, want) in [("32g", Some(32 * GIB)), ("32G", Some(32 * GIB)), ("32GB", Some(32 * GIB)), ("32Gi", Some(32 * GIB)), ("512m", Some(512 << 20)), ("1.5t", Some(3 << 39)), ("1024", Some(1024)), ("4h", None), ("lots", None), ("-1g", None)] {
            assert_eq!(size(s), want, "{s}");
        }
    }
}
