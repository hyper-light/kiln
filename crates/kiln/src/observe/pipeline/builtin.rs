//! Pipelines kiln carries (`docs/observe.md` §8.2), as Datadog's integrations carry theirs, for
//! what is common: nginx's and Apache's access and error logs (Datadog's own rules), each applied
//! to records of its `source`. Read-only: one is turned off, or copied to be changed.

use super::PipelineDef;

/// Datadog's support rules for the common and combined log formats (nginx's, Apache's).
pub(crate) const ACCESS_SUPPORT: &str = r#"_auth %{notSpace:http.auth:nullIf("-")}
_bytes_written %{integer:network.bytes_written}
_client_ip %{ipOrHost:network.client.ip}
_version HTTP\/%{regex("\\d+\\.\\d+"):http.version}
_url %{notSpace:http.url}
_ident %{notSpace:http.ident:nullIf("-")}
_user_agent %{regex("[^\\\"]*"):http.useragent}
_referer %{notSpace:http.referer}
_status_code %{integer:http.status_code}
_method %{word:http.method}
_date_access %{date("dd/MMM/yyyy:HH:mm:ss Z"):date_access}
_x_forwarded_for %{regex("[^\\\"]*"):http._x_forwarded_for:nullIf("-")}"#;

pub(crate) const ACCESS_MATCH: &str = r#"access.common %{_client_ip} %{_ident} %{_auth} \[%{_date_access}\] "(?>%{_method} |)%{_url}(?> %{_version}|)" %{_status_code} (?>%{_bytes_written}|-)
access.combined %{access.common} (%{number:duration:scale(1000000000)} )?"%{_referer}" "%{_user_agent}"( "%{_x_forwarded_for}")?.*"#;

const NGINX_ERROR: &str = r#"error.format %{date("yyyy/MM/dd HH:mm:ss"):date_access} \[%{word:level}\] %{data:error.message}(, %{data::keyvalue(": ",",")})?"#;

const APACHE_ERROR: &str = r#"error.format \[%{date("EEE MMM dd HH:mm:ss.SSSSSS yyyy"):date_access}\] \[(%{word:apache.module}:)?%{word:level}\] \[pid %{integer:process.pid}(:tid %{integer:process.thread_id})?\]( \[client %{ipOrHost:network.client.ip}(:%{port:network.client.port})?\])? %{data:error.message}"#;

/// The processors that come after either's parser: the time, the status (an HTTP status's
/// category, or an error's level), the URL and the user agent read.
fn after_parse() -> serde_json::Value {
    serde_json::json!([
        { "type": "date-remapper", "name": "Its time", "sources": ["date_access"] },
        { "type": "category-processor", "name": "HTTP status category", "target": "http.status_category", "categories": [
            { "filter": { "query": "@http.status_code:[200 TO 299]" }, "name": "OK" },
            { "filter": { "query": "@http.status_code:[300 TO 399]" }, "name": "notice" },
            { "filter": { "query": "@http.status_code:[400 TO 499]" }, "name": "warning" },
            { "filter": { "query": "@http.status_code:[500 TO 599]" }, "name": "error" }
        ] },
        { "type": "status-remapper", "name": "Its status", "sources": ["http.status_category", "level"] },
        { "type": "url-parser", "name": "Its URL", "sources": ["http.url"], "target": "http.url_details" },
        { "type": "user-agent-parser", "name": "Its user agent", "sources": ["http.useragent"], "target": "http.useragent_details" }
    ])
}

fn pipeline(name: &str, source: &str, match_rules: String, support: &str) -> PipelineDef {
    let mut processors = vec![serde_json::json!({ "type": "grok-parser", "name": format!("{name} logs"), "source": "message", "grok": { "support_rules": support, "match_rules": match_rules } })];
    if let serde_json::Value::Array(rest) = after_parse() {
        processors.extend(rest);
    }
    serde_json::from_value(serde_json::json!({ "name": name, "is_enabled": true, "filter": { "query": format!("source:{source}") }, "processors": processors })).unwrap_or_else(|_| PipelineDef { name: name.to_owned(), is_enabled: false, filter: super::FilterDef::default(), processors: Vec::new() })
}

/// kiln's own pipelines, by id, in their order.
pub fn all() -> Vec<(&'static str, PipelineDef)> {
    vec![("builtin:nginx", pipeline("nginx", "nginx", format!("{ACCESS_MATCH}\n{NGINX_ERROR}"), ACCESS_SUPPORT)), ("builtin:apache", pipeline("Apache httpd", "apache", format!("{ACCESS_MATCH}\n{APACHE_ERROR}"), ACCESS_SUPPORT))]
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use kiln_obs::logs::record::Record;

    fn run(id: &str, source: &str, message: &str) -> Record {
        let (_, def) = all().into_iter().find(|(i, _)| *i == id).unwrap();
        let p = def.compile().unwrap();
        let mut r = Record { ts: 1, severity: 9, service: "web".into(), host: "h".into(), source: source.into(), message: message.into(), ..Record::default() };
        assert!(p.apply(&mut r, 1_792_324_800_000_000_000, &mut super::super::Scratch::default()));
        r
    }

    fn attr<'a>(r: &'a Record, k: &str) -> Option<&'a str> {
        r.attrs.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str())
    }

    #[test]
    fn kiln_s_nginx_pipeline_reads_access_and_error_lines() {
        let r = run("builtin:nginx", "nginx", r#"192.0.2.10 - - [18/Oct/2026:12:00:00 +0000] "POST /api/v1/orders?id=7 HTTP/1.1" 502 157 "-" "curl/8.5.0""#);
        assert_eq!(r.ts, 1_792_324_800_000_000_000);
        assert_eq!(r.severity, 17, "a 502: error");
        for (k, v) in [("http.method", "POST"), ("http.status_code", "502"), ("http.status_category", "error"), ("http.url_details.path", "/api/v1/orders"), ("http.url_details.queryString.id", "7"), ("http.useragent_details.browser.family", "curl"), ("http.useragent_details.device.category", "Bot"), ("network.client.ip", "192.0.2.10")] {
            assert_eq!(attr(&r, k), Some(v), "{k}: {:?}", r.attrs);
        }
        let r = run("builtin:nginx", "nginx", r#"2026/10/18 12:00:00 [error] 8409#8409: *317058 open() "/usr/share/nginx/html/favicon.ico" failed (2: No such file or directory), client: 192.0.2.10, server: localhost, request: "GET /favicon.ico HTTP/1.1", host: "example.com""#);
        assert_eq!((r.severity, attr(&r, "client"), attr(&r, "server"), attr(&r, "request")), (17, Some("192.0.2.10"), Some("localhost"), Some("GET /favicon.ico HTTP/1.1")), "{:?}", r.attrs);
        assert!(attr(&r, "error.message").is_some_and(|m| m.ends_with("(2: No such file or directory)")), "{:?}", r.attrs);
    }

    #[test]
    fn kiln_s_apache_pipeline_reads_access_and_error_lines() {
        let r = run("builtin:apache", "apache", r#"203.0.113.5 - frank [18/Oct/2026:12:00:00 +0000] "GET /index.html HTTP/1.0" 200 2326"#);
        assert_eq!((r.severity, attr(&r, "http.auth"), attr(&r, "http.status_category")), (9, Some("frank"), Some("OK")));
        let r = run("builtin:apache", "apache", "[Sun Oct 18 12:00:00.123456 2026] [core:error] [pid 1234:tid 5678] [client 203.0.113.5:51234] AH00128: File does not exist: /var/www/html/favicon.ico");
        assert_eq!(r.ts, 1_792_324_800_123_000_000);
        assert_eq!((r.severity, attr(&r, "apache.module"), attr(&r, "process.pid"), attr(&r, "network.client.port")), (17, Some("core"), Some("1234"), Some("51234")), "{:?}", r.attrs);
    }
}
