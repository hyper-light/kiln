fn add(a: i32, b: i32) -> i32 {
    a + b
}

fn main() {
    println!("fixture {}", add(1, 2));
}

#[cfg(test)]
mod tests {
    #[test]
    fn adds() {
        assert_eq!(super::add(1, 2), 3);
    }
}
