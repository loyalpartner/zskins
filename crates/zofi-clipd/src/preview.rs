/// Truncated text snippet for list-row display. Picks the first non-empty
/// line, trims it, and ellipsises if longer than `MAX_BYTES`.
pub fn build(s: &str) -> String {
    const MAX_BYTES: usize = 180;
    let first_line = s.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let trimmed = first_line.trim();
    if trimmed.len() <= MAX_BYTES {
        return trimmed.to_string();
    }
    let mut end = MAX_BYTES;
    while !trimmed.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    format!("{}…", &trimmed[..end])
}

pub fn build_from_bytes(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    build(&text)
}
