use serde_json::Value as Json;

/// Перенос `output.output_json`: Python `json.dump` без indent
/// использует разделители (', ', ': ') — воспроизводим это поверх
/// компактной сериализации serde_json, не трогая содержимое строк.
pub fn dumps(data: &Json) -> String {
    let compact = serde_json::to_string(data).expect("JSON serialization cannot fail");
    let mut out = String::with_capacity(compact.len() + 16);
    let mut in_string = false;
    let mut escaped = false;
    for c in compact.chars() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            ':' => out.push_str(": "),
            ',' => out.push_str(", "),
            _ => out.push(c),
        }
    }
    out
}

pub fn output_json(data: &Json) {
    println!("{}", dumps(data));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn matches_python_dump_separators() {
        assert_eq!(
            dumps(&json!({"ok": true, "list": ["a", "b"], "n": {}})),
            r#"{"ok": true, "list": ["a", "b"], "n": {}}"#
        );
    }

    #[test]
    fn does_not_touch_strings() {
        assert_eq!(
            dumps(&json!({"s": "a, b: c", "t": "quote \" inside"})),
            r#"{"s": "a, b: c", "t": "quote \" inside"}"#
        );
    }

    #[test]
    fn unicode_not_escaped() {
        assert_eq!(dumps(&json!({"s": "кухня"})), r#"{"s": "кухня"}"#);
    }
}
