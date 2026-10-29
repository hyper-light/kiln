// Backtraces as a Rust service reports them (std::backtrace::Backtrace's Display, as anyhow's
// `{:?}` and tracing's error layers print it), for kiln's error grouping tests.
use std::backtrace::Backtrace;

struct Cart {
    items: Vec<i64>,
}

impl Cart {
    fn total(&self) -> Result<i64, String> {
        for i in &self.items {
            if *i < 0 {
                return Err(format!("bad item {i} in cart 'c-{}'\n{}", i * 7, Backtrace::force_capture()));
            }
        }
        Ok(self.items.iter().sum())
    }
}

fn checkout(order: i64) -> Result<i64, String> {
    Cart { items: vec![1, 2, -order] }.total()
}

fn parse(s: &str) -> Result<i64, String> {
    s.parse::<i64>().map_err(|e| format!("{e}\n{}", Backtrace::force_capture()))
}

fn main() {
    for (name, r) in [("app", checkout(42)), ("app-again", checkout(7)), ("parse", parse("x1"))] {
        if let Err(e) = r {
            let stack = e.split_once('\n').map_or("", |(_, s)| s);
            print!("---- {name}\n{stack}\n");
        }
    }
    let closure = |v: i64| -> Result<i64, String> { if v == 0 { Err(format!("zero\n{}", Backtrace::force_capture())) } else { Ok(10 / v) } };
    if let Err(e) = [1, 0].into_iter().map(closure).collect::<Result<Vec<_>, _>>() {
        let stack = e.split_once('\n').map_or("", |(_, s)| s);
        print!("---- closure\n{stack}\n");
    }
}
