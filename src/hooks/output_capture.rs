/// Tag a string with round metadata.
pub fn tag_with_round(content: &str, round: u32) -> String {
    format!("[round:{}] {}", round, content)
}

/// Extract round number from a tagged message.
pub fn extract_round(content: &str) -> Option<u32> {
    if content.starts_with("[round:") {
        content.get(7..)?.split(']').next()?.parse().ok()
    } else {
        None
    }
}
