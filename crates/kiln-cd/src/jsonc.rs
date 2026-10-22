//! JSON with comments and trailing commas as plain JSON.

pub fn strip(text: &str) -> String {
    // Comments out, strings kept as they are.
    let chars: Vec<char> = text.chars().collect();
    let mut bare: Vec<char> = Vec::with_capacity(chars.len());
    let mut i = 0usize;
    let mut in_str = false;
    while let Some(&c) = chars.get(i) {
        let next = chars.get(i + 1).copied();
        if in_str {
            bare.push(c);
            if c == '\\' {
                if let Some(n) = next {
                    bare.push(n);
                }
                i += 2;
                continue;
            }
            in_str = c != '"';
            i += 1;
            continue;
        }
        match (c, next) {
            ('"', _) => {
                in_str = true;
                bare.push(c);
                i += 1;
            }
            ('/', Some('/')) => {
                while chars.get(i).is_some_and(|c| *c != '\n') {
                    i += 1;
                }
            }
            ('/', Some('*')) => {
                i += 2;
                while chars.get(i).is_some() && !(chars.get(i) == Some(&'*') && chars.get(i + 1) == Some(&'/')) {
                    i += 1;
                }
                i += 2;
            }
            _ => {
                bare.push(c);
                i += 1;
            }
        }
    }
    // Trailing commas out: a comma whose next non-space character closes a list or an object.
    let mut out = String::with_capacity(bare.len());
    let mut in_str = false;
    let mut escaped = false;
    for (j, c) in bare.iter().enumerate() {
        if in_str {
            out.push(*c);
            if escaped {
                escaped = false;
            } else if *c == '\\' {
                escaped = true;
            } else if *c == '"' {
                in_str = false;
            }
            continue;
        }
        if *c == '"' {
            in_str = true;
        }
        if *c == ',' && bare.get(j + 1..).unwrap_or_default().iter().find(|x| !x.is_whitespace()).is_some_and(|x| *x == '}' || *x == ']') {
            continue;
        }
        out.push(*c);
    }
    out
}
