//! The XML formats: JUnit in its dialects, TRX and xUnit.

use super::xml::{self, Element};
use super::{Case, Outcome, seconds_ms, trace};

/// JUnit XML: `<testsuites>` or one `<testsuite>`, suites nested or not, as Surefire, pytest,
/// jest-junit, go-junit-report and nextest write them.
pub fn junit(text: &str) -> Result<Vec<Case>, String> {
    let root = xml::parse(text)?;
    let mut suites: Vec<&Element> = Vec::new();
    match xml::local(&root.name) {
        "testsuite" => suites.push(&root),
        "testsuites" => {}
        other => return Err(format!("<{other}> isn't JUnit's root: <testsuites> or <testsuite>")),
    }
    root.descendants("testsuite", &mut suites);
    let mut out = Vec::new();
    for s in suites {
        let suite_name = s.attr("name").unwrap_or_default();
        for c in s.all("testcase") {
            out.push(junit_case(c, s, suite_name));
        }
    }
    Ok(out)
}

fn junit_case(c: &Element, suite: &Element, suite_name: &str) -> Case {
    let failure = c.first("failure");
    let error = c.first("error");
    let skipped = c.first("skipped");
    // Surefire's and nextest's reruns: failures before the pass that stood, or every one of them.
    let flaky = c.all("flakyFailure").count() + c.all("flakyError").count();
    let reruns: Vec<&Element> = c.all("rerunFailure").chain(c.all("rerunError")).collect();
    let outcome = if failure.is_some() || !reruns.is_empty() {
        Outcome::Failed
    } else if error.is_some() {
        Outcome::Error
    } else if skipped.is_some() {
        Outcome::Skipped
    } else {
        Outcome::Passed
    };
    let why = failure.or(error).or(skipped).or(reruns.first().copied());
    let message = why.and_then(|w| w.attr("message").map(str::to_owned).filter(|m| !m.is_empty())).or_else(|| why.map(|w| w.text.trim().lines().next().unwrap_or_default().to_owned()).filter(|m| !m.is_empty()));
    let out = [c.first("system-out"), c.first("system-err")].into_iter().flatten().map(|e| e.text.trim()).filter(|t| !t.is_empty()).collect::<Vec<_>>().join("\n");
    let body = why.map(|w| w.text.as_str()).filter(|t| !t.trim().is_empty()).map(str::to_owned).unwrap_or(out);
    Case {
        suite: c.attr("classname").filter(|n| !n.is_empty()).unwrap_or(suite_name).to_owned(),
        name: c.attr("name").unwrap_or_default().to_owned(),
        file: c.attr("file").or_else(|| suite.attr("file")).filter(|f| !f.is_empty()).map(str::to_owned),
        line: c.attr("line").and_then(|l| l.trim().parse().ok()),
        outcome,
        duration_ms: c.attr("time").and_then(seconds_ms),
        message,
        trace: if outcome == Outcome::Passed { None } else { trace(&body) },
        retried: u32::try_from(flaky).unwrap_or(u32::MAX),
    }
}

/// `hh:mm:ss.fffffff`, as TRX writes durations.
fn hms_ms(s: &str) -> Option<u64> {
    let mut parts = s.trim().split(':');
    let (h, m, sec) = (parts.next()?, parts.next()?, parts.next()?);
    let total = h.parse::<f64>().ok()? * 3600.0 + m.parse::<f64>().ok()? * 60.0 + sec.parse::<f64>().ok()?;
    (total.is_finite() && total >= 0.0).then(|| (total * 1000.0).round() as u64)
}

/// TRX (Visual Studio, `dotnet test --logger trx`): `UnitTestResult`s, their classes from
/// `TestDefinitions`.
pub fn trx(text: &str) -> Result<Vec<Case>, String> {
    let root = xml::parse(text)?;
    if xml::local(&root.name) != "TestRun" {
        return Err(format!("<{}> isn't TRX's root, <TestRun>", root.name));
    }
    let mut defs: Vec<&Element> = Vec::new();
    root.descendants("UnitTest", &mut defs);
    let class_of = |id: &str| -> Option<String> { defs.iter().find(|d| d.attr("id") == Some(id)).and_then(|d| d.first("TestMethod")).and_then(|m| m.attr("className")).map(str::to_owned) };
    let mut results: Vec<&Element> = Vec::new();
    root.descendants("UnitTestResult", &mut results);
    Ok(results
        .into_iter()
        .map(|r| {
            let outcome = match r.attr("outcome").unwrap_or_default() {
                "Passed" | "PassedButRunAborted" => Outcome::Passed,
                "Failed" => Outcome::Failed,
                "NotExecuted" | "Inconclusive" | "NotRunnable" | "Pending" | "Disconnected" | "Warning" => Outcome::Skipped,
                _ => Outcome::Error,
            };
            let info = r.first("Output").and_then(|o| o.first("ErrorInfo"));
            let message = info.and_then(|i| i.first("Message")).map(|m| m.text.trim().to_owned()).filter(|m| !m.is_empty());
            let stack = info.and_then(|i| i.first("StackTrace")).map(|s| s.text.as_str()).unwrap_or_default();
            let name = r.attr("testName").unwrap_or_default().to_owned();
            let class = r.attr("testId").and_then(class_of).unwrap_or_default();
            // `testName` is often the class and method; the method alone under its class.
            let name = name.strip_prefix(&format!("{class}.")).map(str::to_owned).unwrap_or(name);
            Case { suite: class, name, file: None, line: None, outcome, duration_ms: r.attr("duration").and_then(hms_ms), message, trace: trace(stack), retried: 0 }
        })
        .collect())
}

/// xUnit.net's v2 XML: `<assemblies><assembly><collection><test>`.
pub fn xunit(text: &str) -> Result<Vec<Case>, String> {
    let root = xml::parse(text)?;
    if !matches!(xml::local(&root.name), "assemblies" | "assembly") {
        return Err(format!("<{}> isn't xUnit's root, <assemblies>", root.name));
    }
    let mut tests: Vec<&Element> = Vec::new();
    root.descendants("test", &mut tests);
    Ok(tests
        .into_iter()
        .map(|t| {
            let outcome = match t.attr("result").unwrap_or_default() {
                "Pass" => Outcome::Passed,
                "Fail" => Outcome::Failed,
                "Skip" | "NotRun" => Outcome::Skipped,
                _ => Outcome::Error,
            };
            let failure = t.first("failure");
            let message = failure.and_then(|f| f.first("message")).map(|m| m.text.trim().to_owned()).or_else(|| t.first("reason").map(|r| r.text.trim().to_owned())).filter(|m| !m.is_empty());
            let stack = failure.and_then(|f| f.first("stack-trace")).map(|s| s.text.as_str()).unwrap_or_default();
            let class = t.attr("type").unwrap_or_default().to_owned();
            let name = t.attr("method").map(str::to_owned).unwrap_or_else(|| t.attr("name").unwrap_or_default().to_owned());
            Case {
                suite: class,
                name,
                file: t.attr("source-file").map(str::to_owned),
                line: t.attr("source-line").and_then(|l| l.parse().ok()),
                outcome,
                duration_ms: t.attr("time").and_then(seconds_ms),
                message,
                trace: trace(stack),
                retried: 0,
            }
        })
        .collect())
}
