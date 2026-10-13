use std::collections::HashMap;

/// Splits one `key = value` line of a settings file.
pub fn parse_config_line(line: &str) -> Option<(String, String)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (key, value) = line.split_once('=')?;
    Some((key.trim().to_string(), value.trim().to_string()))
}

pub fn parse_config(text: &str) -> HashMap<String, String> {
    text.lines().filter_map(parse_config_line).collect()
}
