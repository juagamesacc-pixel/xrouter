pub fn is_free(name: &str) -> bool {
    let n = name.to_lowercase();
    n.contains("[free]")
        || n.contains("(free)")
        || n.ends_with("-free")
        || n.ends_with(":free")
        || n.contains("-free-")
        || n.contains("[free]-")
        || n.contains("(free)-")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn free_cases() {
        assert!(is_free("mimo-v2.5-free"));
        assert!(is_free("deepseek/deepseek-r1:free"));
        assert!(is_free("qwen3-coder-[free]"));
        assert!(is_free("some-model(free)"));
        assert!(is_free("model-[free]-v2"));
        assert!(is_free("MODEL-FREE"));
        assert!(!is_free("gpt-4"));
        assert!(!is_free("claude-sonnet-4"));
        assert!(!is_free("freewheel"));
    }
}
