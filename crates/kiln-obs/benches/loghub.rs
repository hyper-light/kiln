//! Log patterns (`kiln_obs::logs::patterns`, Drain) on Loghub's 16 systems, 2,000 labelled lines
//! each (Zhu et al., "Tools and Benchmarks for Automated Log Parsing", ICSE-SEIP 2019;
//! `docs/observe.md` §12.13).
//!
//! First the port: with the benchmark's own settings (each system's regexes, similarity and depth,
//! `logparser/Drain/benchmark.py`), each system's grouping accuracy must be what Drain's README
//! publishes for it, to the line. Then kiln's: one masking for every system (no rules of theirs)
//! and the similarity and depth that do best over all 16, with the time a line takes.
//!
//! `LOGHUB=/path/to/loghub cargo bench -p kiln-obs --bench loghub` (a checkout of
//! github.com/logpai/loghub: `<System>/<System>_2k.log_structured.csv`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::cast_precision_loss)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use kiln_obs::logs::patterns::{self, Params};

/// The benchmark's settings (`benchmark.py`) and Drain's published accuracy (its README).
struct Setting {
    system: &'static str,
    regex: &'static [&'static str],
    st: f64,
    depth: usize,
    published: f64,
}

const SETTINGS: [Setting; 16] = [
    Setting { system: "HDFS", regex: &[r"blk_-?\d+", r"(\d+\.){3}\d+(:\d+)?"], st: 0.5, depth: 4, published: 0.9975 },
    Setting { system: "Hadoop", regex: &[r"(\d+\.){3}\d+"], st: 0.5, depth: 4, published: 0.9475 },
    Setting { system: "Spark", regex: &[r"(\d+\.){3}\d+", r"\b[KGTM]?B\b", r"([\w-]+\.){2,}[\w-]+"], st: 0.5, depth: 4, published: 0.92 },
    Setting { system: "Zookeeper", regex: &[r"(/|)(\d+\.){3}\d+(:\d+)?"], st: 0.5, depth: 4, published: 0.9665 },
    Setting { system: "BGL", regex: &[r"core\.\d+"], st: 0.5, depth: 4, published: 0.9625 },
    Setting { system: "HPC", regex: &[r"=\d+"], st: 0.5, depth: 4, published: 0.887 },
    Setting { system: "Thunderbird", regex: &[r"(\d+\.){3}\d+"], st: 0.5, depth: 4, published: 0.955 },
    Setting { system: "Windows", regex: &[r"0x.*?\s"], st: 0.7, depth: 5, published: 0.997 },
    Setting { system: "Linux", regex: &[r"(\d+\.){3}\d+", r"\d{2}:\d{2}:\d{2}"], st: 0.39, depth: 6, published: 0.69 },
    Setting { system: "Android", regex: &[r"(/[\w-]+)+", r"([\w-]+\.){2,}[\w-]+", r"\b(\-?\+?\d+)\b|\b0[Xx][a-fA-F\d]+\b|\b[a-fA-F\d]{4,}\b"], st: 0.2, depth: 6, published: 0.911 },
    Setting { system: "HealthApp", regex: &[], st: 0.2, depth: 4, published: 0.78 },
    Setting { system: "Apache", regex: &[r"(\d+\.){3}\d+"], st: 0.5, depth: 4, published: 1.0 },
    Setting { system: "Proxifier", regex: &[r"<\d+\ssec", r"([\w-]+\.)+[\w-]+(:\d+)?", r"\d{2}:\d{2}(:\d{2})*", r"[KGTM]B"], st: 0.6, depth: 3, published: 0.5265 },
    Setting { system: "OpenSSH", regex: &[r"(\d+\.){3}\d+", r"([\w-]+\.){2,}[\w-]+"], st: 0.6, depth: 5, published: 0.7875 },
    Setting { system: "OpenStack", regex: &[r"((\d+\.){3}\d+,?)+", r"/.+?\s", r"\d+"], st: 0.5, depth: 5, published: 0.7325 },
    Setting { system: "Mac", regex: &[r"([\w-]+\.){2,}[\w-]+"], st: 0.7, depth: 6, published: 0.7865 },
];

/// A CSV's rows (RFC 4180: quoted fields, `""` a quote within).
fn csv(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match (quoted, c) {
            (true, '"') if chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            (true, '"') => quoted = false,
            (true, c) => field.push(c),
            (false, '"') => quoted = true,
            (false, ',') => row.push(std::mem::take(&mut field)),
            (false, '\r') => {}
            (false, '\n') => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            (false, c) => field.push(c),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

/// A system's lines' contents and their true groups.
fn labelled(root: &Path, system: &str) -> (Vec<String>, Vec<usize>) {
    let path: PathBuf = root.join(system).join(format!("{system}_2k.log_structured.csv"));
    let rows = csv(&std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display())));
    let head = &rows[0];
    let content = head.iter().position(|h| h == "Content").unwrap();
    let event = head.iter().position(|h| h == "EventId").unwrap();
    let mut ids: HashMap<String, usize> = HashMap::new();
    let mut contents = Vec::new();
    let mut truth = Vec::new();
    for r in rows.iter().skip(1).filter(|r| r.len() > content.max(event)) {
        contents.push(r[content].clone());
        let n = ids.len();
        truth.push(*ids.entry(r[event].clone()).or_insert(n));
    }
    (contents, truth)
}

fn main() {
    let Some(root) = std::env::var_os("LOGHUB").map(PathBuf::from) else {
        println!("LOGHUB=<a checkout of github.com/logpai/loghub> cargo bench -p kiln-obs --bench loghub");
        return;
    };
    let data: Vec<(&Setting, Vec<String>, Vec<usize>)> = SETTINGS.iter().map(|s| {
        let (c, t) = labelled(&root, s.system);
        (s, c, t)
    }).collect();

    // Where kiln's masking errs on a system (LOGHUB_DIAG=<system>): its true events split across
    // groups, or groups holding more than one, largest first, with a line of each.
    if let Ok(system) = std::env::var("LOGHUB_DIAG") {
        let Some((_, contents, truth)) = data.iter().find(|(s, _, _)| s.system == system) else { return };
        let (groups, d) = patterns::parse(contents.iter().map(|c| patterns::tokens(c)), Params::default());
        let mut by_truth: HashMap<usize, Vec<usize>> = HashMap::new();
        for (i, &t) in truth.iter().enumerate() {
            by_truth.entry(t).or_default().push(i);
        }
        let mut wrong: Vec<(usize, &Vec<usize>)> = by_truth.iter().map(|(t, lines)| (*t, lines)).filter(|(_, lines)| {
            let g = groups[lines[0]];
            !lines.iter().all(|&i| groups[i] == g) || groups.iter().filter(|&&x| x == g).count() != lines.len()
        }).collect();
        wrong.sort_by_key(|(_, lines)| std::cmp::Reverse(lines.len()));
        let texts: Vec<String> = d.clusters().iter().map(|c| c.text()).collect();
        let mut text_of: HashMap<usize, String> = HashMap::new();
        for (c, &g) in texts.iter().zip(&patterns::parse(contents.iter().map(|c| patterns::tokens(c)), Params::default()).0) {
            text_of.entry(g).or_insert_with(|| c.clone());
        }
        for (_, lines) in wrong.iter().take(8) {
            let mut ours: Vec<usize> = lines.iter().map(|&i| groups[i]).collect();
            ours.sort_unstable();
            ours.dedup();
            println!("a true event of {} lines, in {} of ours:", lines.len(), ours.len());
            for &g in ours.iter().take(3) {
                let i = lines.iter().copied().find(|&i| groups[i] == g).unwrap();
                println!("    {:>4} in it, e.g. {}", groups.iter().filter(|&&x| x == g).count(), contents[i]);
            }
        }
        return;
    }

    // The port, with the benchmark's settings.
    println!("the port, with the benchmark's settings");
    println!("{:<12} {:>8} {:>10} {:>10} {:>8}", "system", "lines", "here", "published", "diff");
    let mut worst = 0.0f64;
    let mut sum = 0.0;
    for (s, contents, truth) in &data {
        let rx: Vec<regex::Regex> = s.regex.iter().map(|r| regex::Regex::new(r).unwrap()).collect();
        let lines: Vec<Vec<String>> = contents
            .iter()
            .map(|c| {
                let mut line = c.clone();
                for r in &rx {
                    line = r.replace_all(&line, patterns::WILDCARD).into_owned();
                }
                line.split_whitespace().map(str::to_owned).collect()
            })
            .collect();
        let (groups, _) = patterns::parse(lines, Params::reference(s.depth, s.st));
        let ga = patterns::grouping_accuracy(truth, &groups);
        sum += ga;
        worst = worst.max((ga - s.published).abs());
        println!("{:<12} {:>8} {:>10.4} {:>10.4} {:>8.4}", s.system, contents.len(), ga, s.published, ga - s.published);
    }
    println!("average {:.4} (published 0.8650); largest difference {worst:.4}", sum / data.len() as f64);

    // kiln's: one masking for all, a grid of similarities and depths.
    println!("\nkiln's masking, one setting for every system: the average grouping accuracy");
    let masked: Vec<Vec<Vec<String>>> = data.iter().map(|(_, contents, _)| contents.iter().map(|c| patterns::tokens(c).into_iter().map(|t| t.into_owned()).collect()).collect()).collect();
    let depths = [3usize, 4, 5, 6, 7];
    print!("{:<10}", "similarity");
    for d in depths {
        print!(" {:>9}", format!("depth {d}"));
    }
    println!();
    let mut best = (0.0, Params::default());
    // Each setting's accuracy on each system, for the cross-validation below.
    let mut grid: Vec<(Params, Vec<f64>)> = Vec::new();
    for st in [0.3, 0.4, 0.5, 0.6, 0.7, 0.8] {
        print!("{st:<10}");
        for depth in depths {
            let p = Params { depth, similarity: st, ..Params::default() };
            let each: Vec<f64> = data.iter().zip(&masked).map(|((_, _, truth), lines)| patterns::grouping_accuracy(truth, &patterns::parse(lines.clone(), p).0)).collect();
            let avg = each.iter().sum::<f64>() / data.len() as f64;
            if avg > best.0 {
                best = (avg, p);
            }
            print!(" {avg:>9.4}");
            grid.push((p, each));
        }
        println!();
    }
    // Leave one out: each system scored with the setting best on the other fifteen (what a system
    // kiln has never seen gets).
    let mut held = 0.0;
    let mut chosen: Vec<String> = Vec::new();
    for i in 0..data.len() {
        let (p, each) = grid
            .iter()
            .max_by(|a, b| {
                let others = |e: &Vec<f64>| e.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, x)| x).sum::<f64>();
                others(&a.1).total_cmp(&others(&b.1))
            })
            .unwrap();
        held += each[i];
        chosen.push(format!("{}:{}/{}", data[i].0.system, p.depth, p.similarity));
    }
    println!("leave one out: {:.4} on the system held out, its setting the other fifteen's best ({})", held / data.len() as f64, chosen.join(" "));
    let p = Params::default();
    println!("\nthe default ({:?}), each system; the best of the grid {:.4} at {:?}", p, best.0, best.1);
    println!("{:<12} {:>10} {:>10} {:>12}", "system", "kiln", "Drain's", "ns a line");
    let mut sum = 0.0;
    for ((s, contents, truth), _) in data.iter().zip(&masked) {
        let t = Instant::now();
        let (groups, d) = patterns::parse(contents.iter().map(|c| patterns::tokens(c)), p);
        let ns = t.elapsed().as_nanos() as f64 / contents.len() as f64;
        let ga = patterns::grouping_accuracy(truth, &groups);
        sum += ga;
        println!("{:<12} {:>10.4} {:>10.4} {:>12.0}  {} templates", s.system, ga, s.published, ns, d.clusters().len());
    }
    println!("average {:.4}", sum / data.len() as f64);
}
