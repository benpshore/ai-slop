fn greeting() -> String {
    String::from("Hello from text-processing-engine!")
}

fn main() {
    println!("{}", greeting());
}

#[cfg(test)]
mod tests {
    use super::greeting;

    #[test]
    fn greets() {
        assert!(greeting().contains("text-processing-engine"));
    }
}
