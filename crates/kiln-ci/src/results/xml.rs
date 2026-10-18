//! Just enough XML for test reports (JUnit, TRX, xUnit): elements, attributes, text, CDATA and
//! the five entities and character references, read into a tree. Comments, processing
//! instructions and a DOCTYPE are skipped; namespaces stay in names (`a:b`), which the formats
//! here don't use. Malformed input is an error with its byte offset, never a panic.

/// An element.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Element {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Element>,
    /// Its text and CDATA, joined.
    pub text: String,
}

impl Element {
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    /// Children named `name` (its local name: a prefix is ignored).
    pub fn all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Element> + 'a {
        self.children.iter().filter(move |c| local(&c.name) == name)
    }

    pub fn first(&self, name: &str) -> Option<&Element> {
        self.children.iter().find(|c| local(&c.name) == name)
    }

    /// Every descendant named `name`, depth first.
    pub fn descendants<'a>(&'a self, name: &'a str, out: &mut Vec<&'a Element>) {
        for c in &self.children {
            if local(&c.name) == name {
                out.push(c);
            }
            c.descendants(name, out);
        }
    }
}

/// A name without its prefix.
pub fn local(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

/// The most nested elements a report may have.
const MAX_DEPTH: usize = 256;

/// The document's root element.
pub fn parse(text: &str) -> Result<Element, String> {
    let b = text.as_bytes();
    let mut i = 0usize;
    let mut stack: Vec<Element> = Vec::new();
    let mut root: Option<Element> = None;
    while i < b.len() {
        if b.get(i) == Some(&b'<') {
            let rest = text.get(i..).unwrap_or_default();
            if rest.starts_with("<!--") {
                i += rest.find("-->").ok_or_else(|| format!("a comment at {i} isn't closed"))? + 3;
            } else if rest.starts_with("<![CDATA[") {
                let end = rest.find("]]>").ok_or_else(|| format!("CDATA at {i} isn't closed"))?;
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(rest.get(9..end).unwrap_or_default());
                }
                i += end + 3;
            } else if rest.starts_with("<?") {
                i += rest.find("?>").ok_or_else(|| format!("a processing instruction at {i} isn't closed"))? + 2;
            } else if rest.starts_with("<!") {
                // A DOCTYPE, with an internal subset in brackets perhaps.
                let mut depth = 0i32;
                let mut j = 2;
                loop {
                    match rest.as_bytes().get(j) {
                        None => return Err(format!("a declaration at {i} isn't closed")),
                        Some(b'[') => depth += 1,
                        Some(b']') => depth -= 1,
                        Some(b'>') if depth <= 0 => break,
                        _ => {}
                    }
                    j += 1;
                }
                i += j + 1;
            } else if rest.starts_with("</") {
                let end = rest.find('>').ok_or_else(|| format!("a closing tag at {i} isn't closed"))?;
                let name = rest.get(2..end).unwrap_or_default().trim();
                let el = stack.pop().ok_or_else(|| format!("</{name}> at {i} closes nothing"))?;
                if el.name != name {
                    return Err(format!("</{name}> at {i} closes <{}>", el.name));
                }
                match stack.last_mut() {
                    Some(parent) => parent.children.push(el),
                    None => root = Some(el),
                }
                i += end + 1;
            } else {
                let (el, used, closed) = open_tag(rest).map_err(|e| format!("{e} at {i}"))?;
                i += used;
                if closed {
                    match stack.last_mut() {
                        Some(parent) => parent.children.push(el),
                        None => root = Some(el),
                    }
                } else {
                    if stack.len() >= MAX_DEPTH {
                        return Err(format!("elements nest deeper than {MAX_DEPTH} at {i}"));
                    }
                    stack.push(el);
                }
            }
            if root.is_some() && stack.is_empty() {
                // Anything after the root but comments and space is ignored, as parsers do.
                break;
            }
        } else {
            let next = text.get(i..).and_then(|r| r.find('<')).map_or(b.len(), |n| i + n);
            if let Some(top) = stack.last_mut() {
                top.text.push_str(&unescape(text.get(i..next).unwrap_or_default()));
            }
            i = next;
        }
    }
    if let Some(open) = stack.last() {
        return Err(format!("<{}> isn't closed", open.name));
    }
    root.ok_or_else(|| "no element".to_owned())
}

/// `<name attr="v" …>` or `<name … />`: the element, the bytes it took, whether it closed itself.
fn open_tag(s: &str) -> Result<(Element, usize, bool), String> {
    let b = s.as_bytes();
    let mut i = 1;
    let start = i;
    while b.get(i).is_some_and(|c| !c.is_ascii_whitespace() && *c != b'>' && *c != b'/') {
        i += 1;
    }
    let name = s.get(start..i).unwrap_or_default().to_owned();
    if name.is_empty() {
        return Err("a tag without a name".into());
    }
    let mut el = Element { name, ..Element::default() };
    loop {
        while b.get(i).is_some_and(u8::is_ascii_whitespace) {
            i += 1;
        }
        match b.get(i) {
            None => return Err(format!("<{}> isn't closed", el.name)),
            Some(b'>') => return Ok((el, i + 1, false)),
            Some(b'/') if b.get(i + 1) == Some(&b'>') => return Ok((el, i + 2, true)),
            Some(_) => {
                let k0 = i;
                while b.get(i).is_some_and(|c| *c != b'=' && !c.is_ascii_whitespace() && *c != b'>' && *c != b'/') {
                    i += 1;
                }
                let key = s.get(k0..i).unwrap_or_default().to_owned();
                while b.get(i).is_some_and(u8::is_ascii_whitespace) {
                    i += 1;
                }
                if b.get(i) != Some(&b'=') {
                    // An attribute without a value (HTML-ish): taken as empty.
                    if key.is_empty() {
                        return Err(format!("a stray character in <{}>", el.name));
                    }
                    el.attrs.push((key, String::new()));
                    continue;
                }
                i += 1;
                while b.get(i).is_some_and(u8::is_ascii_whitespace) {
                    i += 1;
                }
                let quote = *b.get(i).ok_or_else(|| format!("<{}> ends in an attribute", el.name))?;
                if quote != b'"' && quote != b'\'' {
                    return Err(format!("attribute `{key}` of <{}> isn't quoted", el.name));
                }
                i += 1;
                let v0 = i;
                while b.get(i).is_some_and(|c| *c != quote) {
                    i += 1;
                }
                if b.get(i).is_none() {
                    return Err(format!("attribute `{key}` of <{}> isn't closed", el.name));
                }
                el.attrs.push((key, unescape(s.get(v0..i).unwrap_or_default())));
                i += 1;
            }
        }
    }
}

/// Text with its entities and character references replaced (unknown ones are kept as they are).
pub fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find('&') {
        out.push_str(rest.get(..at).unwrap_or_default());
        let tail = rest.get(at..).unwrap_or_default();
        let Some(end) = tail.find(';').filter(|e| *e <= 12) else {
            out.push('&');
            rest = tail.get(1..).unwrap_or_default();
            continue;
        };
        let ent = tail.get(1..end).unwrap_or_default();
        let ch = match ent {
            "lt" => Some('<'),
            "gt" => Some('>'),
            "amp" => Some('&'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => ent
                .strip_prefix("#x")
                .or_else(|| ent.strip_prefix("#X"))
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| ent.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match ch {
            Some(c) => out.push(c),
            None => out.push_str(tail.get(..=end).unwrap_or_default()),
        }
        rest = tail.get(end + 1..).unwrap_or_default();
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elements_attributes_text_and_entities() {
        let doc = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE x [ <!ENTITY e "v"> ]>
<!-- a comment -->
<a k='1' j="&lt;two&gt; &amp; &#x41;&#66;">
  <b/>
  <c>text &quot;q&quot;<![CDATA[ <raw> & ]]></c>
</a>"#;
        let a = parse(doc).unwrap_or_default();
        assert_eq!(a.name, "a");
        assert_eq!(a.attr("k"), Some("1"));
        assert_eq!(a.attr("j"), Some("<two> & AB"));
        assert_eq!(a.children.len(), 2);
        assert_eq!(a.first("c").map(|c| c.text.as_str()), Some("text \"q\" <raw> & "));
        assert!(a.first("b").is_some_and(|b| b.children.is_empty()));
    }

    #[test]
    fn malformed_is_an_error() {
        for bad in ["<a>", "<a></b>", "</a>", "<a k=1/>", "<a k=\"1/>", "", "<!-- open", "<a><![CDATA[x</a>"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        assert_eq!(unescape("a &unknown; & b"), "a &unknown; & b");
    }
}
