//! W29-R24: a derived Dockerfile with immutable base images, using kiln's existing reader.
//! The source bytes stay untouched. Unresolved image arguments fail instead of picking a base.
//! Docker's syntax: https://docs.docker.com/reference/dockerfile/#from

use std::collections::{BTreeMap, BTreeSet};

use crate::wf::diag::{Diag, Pos, Range};
use super::execution::pin_image;

const MAX_BYTES: usize = 1 << 20;
const MAX_ITEMS: usize = 4096;

struct Edit { from: usize, to: usize, replacement: String }
struct Analysis { images: BTreeSet<String>, instructions: Vec<(u32, u32, String, String)>, syntax: Vec<(usize, String)>, tokens: BTreeMap<(u32, u32), Vec<(String, String)>> }

fn error(code: &'static str, line: u32, message: impl Into<String>) -> Diag {
    let at = Pos { line: line.max(1), col: 1, offset: 0 };
    Diag::error(code, message, Range { start: at, end: at }).at("Dockerfile")
}

fn expand(word: &str, args: &BTreeMap<String, String>) -> Result<String, String> {
    let mut out = String::new();
    let mut chars = word.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' { out.push(c); continue; }
        let braced = chars.peek() == Some(&'{');
        if braced { chars.next(); }
        let mut name = String::new();
        while chars.peek().is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_') { if let Some(c) = chars.next() { name.push(c); } }
        if name.is_empty() || (braced && chars.next() != Some('}')) { return Err("resolve this image's Dockerfile argument before locking".into()); }
        out.push_str(args.get(&name).ok_or_else(|| format!("Dockerfile argument {name} has no fixed default"))?);
        if out.len() > 4096 { return Err("expanded image reference exceeds 4096 bytes".into()); }
    }
    if out.contains('$') { return Err("resolve nested Dockerfile image arguments before locking".into()); }
    Ok(out)
}

fn analyze(source: &str) -> Result<Analysis, Vec<Diag>> {
    if source.len() > MAX_BYTES { return Err(vec![error("KCI1022", 1, "Dockerfile exceeds 1 MiB")]); }
    let parsed = crate::dockerfile::parse(source);
    if parsed.diagnostics.iter().any(Diag::is_error) {
        return Err(parsed.diagnostics.into_iter().filter(Diag::is_error).map(|mut d| { d.code = "KCI1022"; d.path = "Dockerfile".into(); d }).collect());
    }
    if parsed.instructions.len() > MAX_ITEMS { return Err(vec![error("KCI1022", 1, "Dockerfile exceeds 4096 instructions")]); }
    let (mut images, mut syntax, mut instructions, mut tokens) = (BTreeSet::new(), Vec::new(), Vec::new(), BTreeMap::<_, Vec<_>>::new());
    // Effective parser directives stop at the first empty line or ordinary comment. Refuse
    // an inert escape directive the existing reader might otherwise interpret differently.
    let (mut active, mut reader_active) = (true, true);
    for (index, line) in source.lines().enumerate() {
        let text = line.trim();
        let Some(body) = text.strip_prefix('#') else { break };
        let Some((key, value)) = body.split_once('=') else { reader_active = false; active = false; continue };
        let (key, value) = (key.trim().to_ascii_lowercase(), value.trim());
        if reader_active && !active && key == "escape" { return Err(vec![error("KCI4050", u32::try_from(index.saturating_add(1)).unwrap_or(u32::MAX), "move the escape directive above ordinary comments before locking")]); }
        if active && !matches!(key.as_str(), "syntax" | "escape" | "check") { active = false; }
        if active && key == "syntax" {
            if !syntax.is_empty() { return Err(vec![error("KCI1022", 1, "duplicate syntax directive")]); }
            images.insert(value.to_owned()); syntax.push((index, value.to_owned()));
        }
    }
    let (mut args, mut stages) = (BTreeMap::new(), BTreeSet::new());
    let mut stage_count = 0usize;
    for instruction in parsed.instructions {
        let fail = |message| vec![error("KCI4050", instruction.line, message)];
        if instruction.keyword == "ARG" && stage_count == 0 {
            let (name, value) = instruction.args.split_once('=').map_or((instruction.args.as_str(), None), |(n, v)| (n, Some(v)));
            if !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') && let Some(value) = value {
                let value = value.trim();
                let value = value.strip_prefix('"').and_then(|s| s.strip_suffix('"')).or_else(|| value.strip_prefix('\'').and_then(|s| s.strip_suffix('\''))).unwrap_or(value);
                args.insert(name.into(), expand(value, &args).map_err(fail)?);
            }
            continue;
        }
        if matches!(instruction.keyword.as_str(), "COPY" | "RUN") {
            for flag in instruction.args.split_whitespace().take_while(|s| s.starts_with("--")) {
                let image = if instruction.keyword == "COPY" { flag.strip_prefix("--from=") }
                    else { flag.strip_prefix("--mount=").and_then(|s| s.split(',').find_map(|s| s.strip_prefix("from="))) };
                let Some(image) = image else { continue };
                if stages.contains(&image.to_ascii_lowercase()) || image.parse::<usize>().is_ok_and(|n| n < stage_count) { continue; }
                if image.contains('$') || image.is_empty() { return Err(fail("resolve this external image argument before locking".into())); }
                images.insert(image.into());
                tokens.entry((instruction.line, instruction.end_line)).or_default().push((flag.into(), image.into()));
            }
        }
        if instruction.keyword != "FROM" { continue; }
        let words: Vec<_> = instruction.args.split_whitespace().collect();
        let flags = words.iter().take_while(|w| w.starts_with("--")).count();
        if words.iter().take(flags).any(|w| !w.starts_with("--platform=")) { return Err(fail("unsupported FROM flag; resolve it before locking".into())); }
        let raw = words.get(flags).ok_or_else(|| fail("FROM needs its image".into()))?;
        let image = expand(raw, &args).map_err(fail)?;
        let tail = words.get(flags.saturating_add(1)..).unwrap_or_default();
        if !(tail.is_empty() || tail.len() == 2 && tail.first().is_some_and(|s| s.eq_ignore_ascii_case("AS"))) { return Err(fail("FROM's image or stage alias is unresolved".into())); }
        let alias = tail.get(1).copied();
        let internal = image.eq_ignore_ascii_case("scratch") || stages.contains(&image.to_ascii_lowercase());
        if !internal { images.insert(image.clone()); }
        let prefix = words.iter().take(flags).copied().collect::<Vec<_>>().join(" ");
        let prefix = if prefix.is_empty() { "FROM ".into() } else { format!("FROM {prefix} ") };
        let suffix = alias.map(|a| format!(" AS {a}")).unwrap_or_default();
        instructions.push((instruction.line, instruction.end_line, if internal { String::new() } else { image.clone() }, format!("{prefix}{image}{suffix}")));
        if let Some(alias) = alias { stages.insert(alias.to_ascii_lowercase()); }
        stage_count = stage_count.saturating_add(1);
    }
    for image in &images {
        let placeholder = format!("sha256:{}", "0".repeat(64));
        let digest = image.split_once('@').map_or(placeholder.as_str(), |(_, d)| d);
        pin_image(image, digest).map_err(|m| vec![error("KCI4050", 1, m)])?;
    }
    Ok(Analysis { images, instructions, syntax, tokens })
}

pub fn images(source: &str) -> Result<Vec<String>, Vec<Diag>> { analyze(source).map(|a| a.images.into_iter().collect()) }

pub fn rewrite(source: &str, pins: &BTreeMap<String, String>) -> Result<String, Vec<Diag>> {
    let analysis = analyze(source)?;
    let mut resolved = BTreeMap::new();
    for image in analysis.images {
        let pinned = pins.get(&image).ok_or_else(|| vec![error("KCI4050", 1, format!("{image}: Dockerfile image is missing from the snapshot"))])?;
        let digest = pinned.split_once('@').map(|(_, d)| d).unwrap_or_default();
        if pin_image(&image, digest).map_err(|m| vec![error("KCI1022", 1, m)])? != *pinned { return Err(vec![error("KCI4054", 1, format!("{image}: the image snapshot names another source"))]); }
        resolved.insert(image, pinned.clone());
    }
    let mut offsets = Vec::new();
    let mut offset = 0usize;
    for line in source.split_inclusive('\n') { offsets.push(offset); offset = offset.saturating_add(line.len()); }
    offsets.push(source.len());
    let mut edits = Vec::new();
    for (line, end, image, replacement) in analysis.instructions {
        if image.is_empty() { continue; }
        let (start, end) = (usize::try_from(line.saturating_sub(1)).unwrap_or(usize::MAX), usize::try_from(end).unwrap_or(usize::MAX));
        let (Some(from), Some(to)) = (offsets.get(start), offsets.get(end)) else { return Err(vec![error("KCI1022", line, "Dockerfile source range is invalid")]) };
        let pinned = resolved.get(&image).ok_or_else(|| vec![error("KCI4050", line, "Dockerfile image wasn't resolved")])?;
        // Replace only the base token, preserving the platform flag and stage alias.
        let replacement = replacement.replacen(&format!(" {image}"), &format!(" {pinned}"), 1);
        edits.push(Edit { from: *from, to: *to, replacement: format!("{replacement}{}", if source.get(*from..*to).is_some_and(|s| s.ends_with('\n')) { "\n" } else { "" }) });
    }
    for (line, image) in analysis.syntax {
        let (Some(from), Some(to)) = (offsets.get(line), offsets.get(line.saturating_add(1))) else { return Err(vec![error("KCI1022", 1, "syntax directive has no source range")]) };
        let pinned = resolved.get(&image).ok_or_else(|| vec![error("KCI4050", 1, "syntax image wasn't resolved")])?;
        edits.push(Edit { from: *from, to: *to, replacement: format!("# syntax={pinned}\n") });
    }
    for ((line, end), tokens) in analysis.tokens {
        let (start, end) = (usize::try_from(line.saturating_sub(1)).unwrap_or(usize::MAX), usize::try_from(end).unwrap_or(usize::MAX));
        let (Some(from), Some(to)) = (offsets.get(start), offsets.get(end)) else { return Err(vec![error("KCI1022", line, "external image has no source range")]) };
        let mut replacement = source.get(*from..*to).ok_or_else(|| vec![error("KCI1022", line, "external image is outside its source")])?.to_owned();
        for (token, image) in tokens {
            if replacement.matches(&token).count() != 1 { return Err(vec![error("KCI4050", line, "external image flag is ambiguous across continued lines or heredocs")]); }
            let pinned = resolved.get(&image).ok_or_else(|| vec![error("KCI4050", line, "external image wasn't resolved")])?;
            let next = if token.starts_with("--from=") { format!("--from={pinned}") } else {
                let fields = token.strip_prefix("--mount=").unwrap_or_default().split(',')
                    .map(|part| if part.strip_prefix("from=") == Some(image.as_str()) { format!("from={pinned}") } else { part.into() }).collect::<Vec<String>>();
                format!("--mount={}", fields.join(","))
            };
            replacement = replacement.replacen(&token, &next, 1);
        }
        edits.push(Edit { from: *from, to: *to, replacement });
    }
    edits.sort_by_key(|e| std::cmp::Reverse(e.from));
    let mut output = source.to_owned();
    for edit in edits {
        if output.get(edit.from..edit.to).is_none() { return Err(vec![error("KCI1022", 1, "Dockerfile edit is outside its source")]); }
        output.replace_range(edit.from..edit.to, &edit.replacement);
    }
    if output.len() > MAX_BYTES { return Err(vec![error("KCI1022", 1, "derived Dockerfile exceeds 1 MiB")]); }
    Ok(output)
}

#[cfg(test)]
mod tests;
