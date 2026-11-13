use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Check {
    Pending,
    Doing,
    Done,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub text: String,
    pub check: Check,
}

pub fn items(args: &Value) -> Result<Vec<Item>, String> {
    let list = args
        .get("items")
        .or_else(|| args.get("todos"))
        .and_then(Value::as_array)
        .ok_or("missing `items`, a list")?;
    list.iter()
        .enumerate()
        .map(|(i, item)| {
            let text = item
                .get("text")
                .or_else(|| item.get("content"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .ok_or_else(|| format!("items[{i}] has no `text`"))?;
            let check = match item.get("status").and_then(Value::as_str).unwrap_or("pending") {
                "done" | "completed" => Check::Done,
                "in_progress" | "doing" => Check::Doing,
                "pending" | "todo" => Check::Pending,
                other => {
                    return Err(format!("{other:?} is not a status: pending, in_progress or done"));
                }
            };
            Ok(Item { text: text.to_string(), check })
        })
        .collect()
}

pub fn write(args: &Value) -> Result<String, String> {
    let items = items(args)?;
    let mut lines: Vec<String> = items
        .iter()
        .map(|i| {
            let mark = match i.check {
                Check::Done => "[x]",
                Check::Doing => "[~]",
                Check::Pending => "[ ]",
            };
            format!("{mark} {}", i.text)
        })
        .collect();
    let doing = items.iter().filter(|i| i.check == Check::Doing).count();
    let left = items.iter().filter(|i| i.check != Check::Done).count();
    let note = match (doing, left) {
        (_, 0) if !items.is_empty() => Some("All done."),
        (0, _) => Some("Nothing is marked in_progress."),
        (1, _) => None,
        _ => Some("More than one item is in progress; keep to one."),
    };
    if let Some(n) = note {
        lines.push(String::new());
        lines.push(n.to_string());
    }
    Ok(lines.join("\n"))
}

pub fn summary(args: &Value) -> Option<String> {
    let items = items(args).ok()?;
    let done = items.iter().filter(|i| i.check == Check::Done).count();
    Some(format!("{done} of {} done", items.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_list_comes_back_marked_with_a_nudge_when_it_drifts() {
        let out = write(&json!({"items": [
            {"text": "read", "status": "done"},
            {"text": "write", "status": "in_progress"},
            {"text": "test"},
        ]}))
        .unwrap();
        assert_eq!(out, "[x] read\n[~] write\n[ ] test");
        let none = write(&json!({"items": [{"text": "a"}]})).unwrap();
        assert!(none.ends_with("Nothing is marked in_progress."));
        let two = write(&json!({"items": [
            {"text": "a", "status": "doing"}, {"text": "b", "status": "in_progress"}
        ]}))
        .unwrap();
        assert!(two.ends_with("keep to one."));
        let all = write(&json!({"items": [{"text": "a", "status": "completed"}]})).unwrap();
        assert!(all.ends_with("All done."));
    }

    #[test]
    fn bad_lists_are_refused_with_the_reason() {
        assert!(write(&json!({})).unwrap_err().contains("items"));
        assert!(write(&json!({"items": [{"status": "done"}]})).unwrap_err().contains("items[0]"));
        assert!(
            write(&json!({"items": [{"text": "a", "status": "weird"}]}))
                .unwrap_err()
                .contains("weird")
        );
    }

    #[test]
    fn the_summary_counts_what_is_done() {
        let args = json!({"todos": [{"content": "a", "status": "done"}, {"content": "b"}]});
        assert_eq!(summary(&args).as_deref(), Some("1 of 2 done"));
    }
}
