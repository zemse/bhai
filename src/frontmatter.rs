//! A small hand-written parser for the YAML frontmatter of skill and agent files. Only
//! top-level scalars and lists are read; everything else is ignored.

/// Split `text` into its frontmatter lines and body. `None` without a closed `---` block.
pub fn split(text: &str) -> Option<(Vec<&str>, &str)> {
    let rest = text.strip_prefix("---")?;
    let rest = rest
        .strip_prefix("\r\n")
        .or_else(|| rest.strip_prefix('\n'))?;
    let mut front = Vec::new();
    let mut offset = text.len() - rest.len();
    for line in rest.split_inclusive('\n') {
        offset += line.len();
        if line.trim_end() == "---" {
            return Some((front, text[offset..].trim_start_matches(['\n', '\r'])));
        }
        front.push(line.trim_end_matches(['\n', '\r']));
    }
    None
}

/// A top-level key's value: plain, quoted, a `|`/`>` block, or continued on indented lines.
pub fn value(lines: &[&str], key: &str) -> Option<String> {
    let (first, more) = entry(lines, key)?;
    if first.starts_with(['|', '>']) {
        let separator = if first.starts_with('|') { "\n" } else { " " };
        let lines: Vec<&str> = more.into_iter().filter(|l| !l.is_empty()).collect();
        return Some(lines.join(separator));
    }
    if let Some(quoted) = unquote(first) {
        return Some(quoted);
    }
    let mut words = vec![first];
    words.extend(more.into_iter().filter(|l| !l.is_empty()));
    Some(words.join(" ").trim().to_string())
}

/// A top-level list: `a, b`, `[a, b]` or `- a` items on the following lines.
pub fn list(lines: &[&str], key: &str) -> Option<Vec<String>> {
    let (first, more) = entry(lines, key)?;
    let items: Vec<String> = if first.is_empty() {
        more.iter()
            .filter_map(|l| l.strip_prefix('-'))
            .map(item)
            .collect()
    } else {
        let inner = first
            .strip_prefix('[')
            .and_then(|f| f.strip_suffix(']'))
            .unwrap_or(first);
        inner.split(',').map(item).collect()
    };
    Some(items.into_iter().filter(|i| !i.is_empty()).collect())
}

/// A top-level list of mappings: the lines of each `- key: value` block, dedented to
/// the first item key so `value` and `list` read them as their own frontmatter.
pub fn items<'a>(lines: &[&'a str], key: &str) -> Option<Vec<Vec<&'a str>>> {
    let index = position(lines, key)?;
    let mut blocks: Vec<Vec<&'a str>> = Vec::new();
    let mut indent = 0;
    for line in &lines[index + 1..] {
        if line.trim().is_empty() {
            if let Some(block) = blocks.last_mut() {
                block.push("");
            }
            continue;
        }
        let trimmed = line.trim_start();
        let dash = line.len() - trimmed.len();
        if trimmed.starts_with("- ") && (blocks.is_empty() || dash < indent) {
            let rest = trimmed[1..].trim_start();
            indent = line.len() - rest.len();
            blocks.push(vec![rest]);
        } else if line.starts_with([' ', '\t']) {
            let Some(block) = blocks.last_mut() else {
                continue;
            };
            let cut = indent.min(line.len() - trimmed.len());
            block.push(&line[cut..]);
        } else {
            break;
        }
    }
    Some(blocks)
}

/// One list item, unquoted.
fn item(text: &str) -> String {
    let text = text.trim();
    unquote(text).unwrap_or_else(|| text.to_string())
}

/// The inline value after `key:` and the trimmed indented lines that follow it.
fn entry<'a>(lines: &[&'a str], key: &str) -> Option<(&'a str, Vec<&'a str>)> {
    let index = position(lines, key)?;
    let first = lines[index][key.len()..].trim_start()[1..].trim();
    let more = lines[index + 1..]
        .iter()
        .take_while(|line| line.trim().is_empty() || line.starts_with([' ', '\t', '-']))
        .map(|line| line.trim())
        .collect();
    Some((first, more))
}

/// The line `key:` is on.
fn position(lines: &[&str], key: &str) -> Option<usize> {
    lines.iter().position(|line| {
        line.strip_prefix(key)
            .is_some_and(|rest| rest.trim_start().starts_with(':'))
    })
}

/// The contents of a `"..."` or `'...'` scalar on one line.
fn unquote(value: &str) -> Option<String> {
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        let inner = &value[1..value.len() - 1];
        return Some(inner.replace("\\\"", "\"").replace("\\\\", "\\"));
    }
    if value.len() >= 2 && value.starts_with('\'') && value.ends_with('\'') {
        return Some(value[1..value.len() - 1].replace("''", "'"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lists(text: &str, key: &str) -> Option<Vec<String>> {
        let (front, _) = split(text)?;
        list(&front, key)
    }

    #[test]
    fn lists_inline_flow_and_block() {
        let text = "---\ntools: Read, Write ,Bash\nskills: [\"ios*\", '!swift']\nmcp:\n  - tavily\n  - \"x__*\"\nempty:\nother: 1\n---\nbody";
        assert_eq!(lists(text, "tools").unwrap(), ["Read", "Write", "Bash"]);
        assert_eq!(lists(text, "skills").unwrap(), ["ios*", "!swift"]);
        assert_eq!(lists(text, "mcp").unwrap(), ["tavily", "x__*"]);
        assert_eq!(lists(text, "empty").unwrap(), Vec::<String>::new());
        assert_eq!(lists(text, "missing"), None);
        assert_eq!(split(text).unwrap().1, "body");
        let flush = "---\nskills:\n- '!*'\n- pdf\nname: x\n---\n";
        assert_eq!(lists(flush, "skills").unwrap(), ["!*", "pdf"]);
    }

    #[test]
    fn items_split_a_list_of_mappings() {
        let text = "---\nname: w\nsteps:\n  - id: a\n    prompt: |\n      one\n      two\n  \
- id: b\n    needs:\n      - a\n    prompt: three\nbudget_tokens: 10\n---\nbody";
        let (front, body) = split(text).unwrap();
        let steps = items(&front, "steps").unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(value(&steps[0], "id").unwrap(), "a");
        assert_eq!(value(&steps[0], "prompt").unwrap(), "one\ntwo");
        assert_eq!(list(&steps[1], "needs").unwrap(), ["a"]);
        assert_eq!(value(&steps[1], "prompt").unwrap(), "three");
        assert_eq!(value(&front, "budget_tokens").unwrap(), "10");
        assert_eq!(body, "body");
        assert!(items(&front, "missing").is_none());
    }
}
