//! `submit_result`, registered only in a workflow child whose step declares an
//! `output_contract`. The result is checked against the contract when it is submitted,
//! so a child that got it wrong is told why and corrects it in the same turn, rather
//! than failing the step once the turn is over. Needs no approval.

use std::sync::{Arc, Mutex};

use serde_json::{Map, Value, json};

use super::BoxFuture;
use super::Tool;

pub const NAME: &str = "submit_result";
/// What an accepted submission answers with; the history is read for it afterwards.
pub const ACCEPTED: &str = "Result accepted.";
/// Mismatches listed for one submission; the rest are counted.
const MAX_ERRORS: usize = 10;
/// Characters of a property name shown in a path. The names come from the model.
const MAX_SEGMENT: usize = 64;
/// The types a contract can name.
const TYPES: [&str; 7] = [
    "object", "array", "string", "number", "integer", "boolean", "null",
];
/// The keywords a contract may use. Anything else is refused when the workflow loads,
/// so no one writes a constraint believing it is enforced.
const KEYWORDS: [&str; 9] = [
    "type",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "enum",
    "minItems",
    "maxItems",
    "description",
];

/// The JSON Schema a step's result must match: a closed subset, an object at the top.
#[derive(Debug, Clone, PartialEq)]
pub struct Contract {
    pub schema: Value,
}

impl Contract {
    /// Read a contract from the schema's text. `Err` says what in it is not supported.
    pub fn parse(text: &str) -> Result<Self, String> {
        let schema: Value = serde_json::from_str(text).map_err(|e| format!("is not JSON: {e}"))?;
        if schema.get("type").and_then(Value::as_str) != Some("object") {
            return Err("must have `\"type\": \"object\"` at the top".to_string());
        }
        compile(&schema, "")?;
        Ok(Self { schema })
    }

    /// What is wrong with `value`, as bounded paths and never the values at them.
    pub fn check(&self, value: &Value) -> Vec<String> {
        let mut errors = Errors::default();
        walk(&self.schema, value, &mut String::new(), &mut errors);
        errors.into_lines()
    }
}

/// Refuse anything the validator does not enforce, at the path it is at.
fn compile(schema: &Value, at: &str) -> Result<(), String> {
    let place = || match at {
        "" => "at the top".to_string(),
        at => format!("at `{at}`"),
    };
    let Some(schema) = schema.as_object() else {
        return Err(format!("{} is not a schema object", place()));
    };
    if let Some(key) = schema.keys().find(|k| !KEYWORDS.contains(&k.as_str())) {
        return Err(format!(
            "uses `{key}` {}; a contract takes only {}",
            place(),
            KEYWORDS.join(", ")
        ));
    }
    let kind = match schema.get("type") {
        Some(Value::String(kind)) if TYPES.contains(&kind.as_str()) => kind.as_str(),
        Some(_) => return Err(format!("has a bad `type` {}", place())),
        None => return Err(format!("has no `type` {}", place())),
    };
    if let Some(values) = schema.get("enum") {
        let scalars = values
            .as_array()
            .filter(|v| !v.is_empty())
            .is_some_and(|v| v.iter().all(|v| !v.is_object() && !v.is_array()));
        if !scalars {
            return Err(format!(
                "has an `enum` {} that is not a list of scalars",
                place()
            ));
        }
    }
    let only = |key: &str, wanted: &str| match schema.contains_key(key) && kind != wanted {
        true => Err(format!("has `{key}` {} on a `{kind}`", place())),
        false => Ok(()),
    };
    for key in ["properties", "required", "additionalProperties"] {
        only(key, "object")?;
    }
    for key in ["items", "minItems", "maxItems"] {
        only(key, "array")?;
    }
    if let Some(properties) = schema.get("properties") {
        let Some(properties) = properties.as_object() else {
            return Err(format!(
                "has `properties` {} that is not an object",
                place()
            ));
        };
        for (name, property) in properties {
            compile(property, &format!("{at}/{name}"))?;
        }
    }
    if let Some(required) = schema.get("required") {
        let names = required.as_array().filter(|names| {
            names.iter().all(|name| {
                name.as_str().is_some_and(|name| {
                    schema
                        .get("properties")
                        .is_some_and(|p| p.get(name).is_some())
                })
            })
        });
        if names.is_none() {
            return Err(format!(
                "has `required` {} that names something `properties` does not",
                place()
            ));
        }
    }
    if schema
        .get("additionalProperties")
        .is_some_and(|v| !v.is_boolean())
    {
        return Err(format!(
            "has `additionalProperties` {} that is not true or false",
            place()
        ));
    }
    for key in ["minItems", "maxItems"] {
        if schema.get(key).is_some_and(|v| v.as_u64().is_none()) {
            return Err(format!(
                "has `{key}` {} that is not a whole number",
                place()
            ));
        }
    }
    if let Some(items) = schema.get("items") {
        compile(items, &format!("{at}/*"))?;
    }
    Ok(())
}

#[derive(Default)]
struct Errors {
    lines: Vec<String>,
    more: usize,
}

impl Errors {
    fn push(&mut self, path: &str, what: String) {
        match self.lines.len() < MAX_ERRORS {
            true => self.lines.push(format!(
                "`{}`: {what}",
                match path {
                    "" => "/",
                    path => path,
                }
            )),
            false => self.more += 1,
        }
    }

    fn into_lines(mut self) -> Vec<String> {
        if self.more > 0 {
            self.lines.push(format!("and {} more", self.more));
        }
        self.lines
    }
}

/// The JSON type of a value, as a schema names it.
fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn walk(schema: &Value, value: &Value, path: &mut String, errors: &mut Errors) {
    let wanted = schema["type"].as_str().unwrap_or_default();
    let found = kind(value);
    let fits = match (wanted, value) {
        ("number", Value::Number(_)) => true,
        // `2.0` is an integer as JSON Schema reads it.
        ("integer", Value::Number(n)) => n.as_f64().is_some_and(|f| f.fract() == 0.0),
        _ => wanted == found,
    };
    if !fits {
        errors.push(path, format!("expected {wanted}, got {found}"));
        return;
    }
    if let Some(values) = schema.get("enum").and_then(Value::as_array)
        && !values.contains(value)
    {
        errors.push(path, "is not one of the values `enum` allows".to_string());
    }
    match value {
        Value::Object(object) => fields(schema, object, path, errors),
        Value::Array(items) => {
            let count = items.len() as u64;
            if let Some(min) = schema.get("minItems").and_then(Value::as_u64)
                && count < min
            {
                errors.push(path, format!("has {count} items, fewer than {min}"));
            }
            if let Some(max) = schema.get("maxItems").and_then(Value::as_u64)
                && count > max
            {
                errors.push(path, format!("has {count} items, more than {max}"));
            }
            if let Some(each) = schema.get("items") {
                for (index, item) in items.iter().enumerate() {
                    let len = path.len();
                    path.push_str(&format!("/{index}"));
                    walk(each, item, path, errors);
                    path.truncate(len);
                }
            }
        }
        _ => {}
    }
}

fn fields(schema: &Value, object: &Map<String, Value>, path: &mut String, errors: &mut Errors) {
    let properties = schema.get("properties").and_then(Value::as_object);
    for name in schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if !object.contains_key(name) {
            errors.push(path, format!("is missing required `{name}`"));
        }
    }
    let closed = schema.get("additionalProperties") == Some(&Value::Bool(false));
    for (name, value) in object {
        match properties.and_then(|p| p.get(name)) {
            Some(property) => {
                let len = path.len();
                path.push('/');
                path.push_str(&segment(name));
                walk(property, value, path, errors);
                path.truncate(len);
            }
            None if closed => errors.push(
                path,
                format!("has `{}`, which the contract does not allow", segment(name)),
            ),
            None => {}
        }
    }
}

/// A property name as a path shows it: on one line and cut short.
fn segment(name: &str) -> String {
    let flat: String = name
        .chars()
        .map(|c| match c.is_control() || c == '`' {
            true => ' ',
            false => c,
        })
        .collect();
    match flat.chars().count() > MAX_SEGMENT {
        true => format!("{}…", flat.chars().take(MAX_SEGMENT).collect::<String>()),
        false => flat,
    }
}

/// `value` with every string in it neutralised, as a child's prose report is, since
/// a typed result reaches the next step's prompt by the same road.
pub fn sanitized(value: Value) -> Value {
    match value {
        Value::String(text) => Value::String(super::agent::sanitize(&text)),
        Value::Array(items) => Value::Array(items.into_iter().map(sanitized).collect()),
        Value::Object(object) => Value::Object(
            object
                .into_iter()
                .map(|(name, value)| (name, sanitized(value)))
                .collect(),
        ),
        other => other,
    }
}

pub struct Submit {
    pub contract: Arc<Contract>,
    /// Every accepted result, in the order the calls ran.
    pub accepted: Arc<Mutex<Vec<Value>>>,
}

impl Tool for Submit {
    fn name(&self) -> &str {
        NAME
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "name": NAME,
            "description": "Hand in the result of your task. It is checked against the \
        schema of `output`; a result that does not match it is refused with what is wrong, \
        so fix that and call again. A plain answer is not a result: only an accepted call \
        is. Once it is accepted, end your turn.",
            "strict": false,
            "parameters": {
                "type": "object",
                "properties": {
                    "output": self.contract.schema,
                },
                "required": ["output"],
                "additionalProperties": false
            }
        })
    }

    fn needs_approval(&self) -> bool {
        false
    }

    fn describe(&self, args: &Value) -> Result<String, String> {
        match args.get("output") {
            Some(_) => Ok("submit result".to_string()),
            None => Err("missing required field `output`.".to_string()),
        }
    }

    fn execute<'a>(&'a self, args: &'a Value) -> BoxFuture<'a, (String, bool)> {
        Box::pin(async move {
            let Some(output) = args.get("output") else {
                return ("missing required field `output`.".to_string(), false);
            };
            let errors = self.contract.check(output);
            // A model sometimes sends the object encoded as a string. That one decoding
            // is tolerated when what it decodes to fits; the errors reported are the
            // ones for what was sent.
            let decoded = match (errors.is_empty(), output) {
                (false, Value::String(text)) => serde_json::from_str::<Value>(text)
                    .ok()
                    .filter(|value| self.contract.check(value).is_empty()),
                _ => None,
            };
            if !errors.is_empty() && decoded.is_none() {
                return (
                    format!(
                        "Result refused: `output` does not match its schema.\n- {}\nFix these \
and call {NAME} again.",
                        errors.join("\n- ")
                    ),
                    false,
                );
            }
            self.accepted
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(decoded.unwrap_or_else(|| output.clone()));
            (format!("{ACCEPTED} End your turn now."), true)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contract() -> Contract {
        Contract::parse(
            r#"{"type": "object", "properties": {
                "risky": {"type": "boolean"},
                "severity": {"type": "string", "enum": ["low", "high"]},
                "files": {"type": "array", "items": {"type": "string"}, "maxItems": 2},
                "count": {"type": "integer"}
            }, "required": ["risky", "severity"], "additionalProperties": false}"#,
        )
        .unwrap()
    }

    #[test]
    fn a_result_that_fits_has_no_errors() {
        let ok = json!({"risky": true, "severity": "low", "files": ["a"], "count": 2.0});
        assert_eq!(contract().check(&ok), Vec::<String>::new());
    }

    #[test]
    fn errors_name_the_path_and_never_the_value() {
        let bad = json!({
            "severity": "SECRET-VALUE",
            "files": ["a", 7, "c"],
            "count": 1.5,
            "extra": "SECRET-VALUE",
        });
        let mut errors = contract().check(&bad);
        errors.sort();
        assert_eq!(
            errors,
            [
                "`/`: has `extra`, which the contract does not allow",
                "`/`: is missing required `risky`",
                "`/count`: expected integer, got number",
                "`/files/1`: expected string, got integer",
                "`/files`: has 3 items, more than 2",
                "`/severity`: is not one of the values `enum` allows",
            ]
        );
        assert!(!errors.join("\n").contains("SECRET"));
    }

    #[test]
    fn the_errors_reported_are_bounded() {
        let contract = Contract::parse(
            r#"{"type": "object", "properties": {"a": {"type": "array", "items": {"type": "string"}}}}"#,
        )
        .unwrap();
        let errors = contract.check(&json!({"a": vec![1; 50]}));
        assert_eq!(errors.len(), MAX_ERRORS + 1);
        assert_eq!(errors.last().unwrap(), "and 40 more");
        let long = "k".repeat(500);
        let closed =
            Contract::parse(r#"{"type": "object", "additionalProperties": false}"#).unwrap();
        let errors = closed.check(&json!({ long: 1 }));
        assert!(errors[0].len() < 150, "{}", errors[0]);
    }

    #[test]
    fn a_contract_the_validator_would_not_enforce_is_refused() {
        let bad = |text: &str| Contract::parse(text).unwrap_err();
        assert!(bad("{nope").contains("not JSON"));
        assert!(bad(r#"{"type": "array"}"#).contains("`\"type\": \"object\"`"));
        assert!(
            bad(r#"{"type": "object", "properties": {"a": {"type": "string", "pattern": "x"}}}"#)
                .contains("uses `pattern` at `/a`")
        );
        assert!(
            bad(r#"{"type": "object", "properties": {"a": {}}}"#).contains("no `type` at `/a`")
        );
        assert!(bad(r#"{"type": "object", "required": ["a"]}"#).contains("`required`"));
        assert!(bad(r#"{"type": "object", "properties": {"a": {"type": "string", "items": {"type": "string"}}}}"#)
            .contains("`items` at `/a` on a `string`"));
    }

    #[tokio::test]
    async fn a_refused_result_says_why_and_an_accepted_one_is_kept() {
        let submit = Submit {
            contract: Arc::new(contract()),
            accepted: Arc::default(),
        };
        let (out, ok) = submit.execute(&json!({"output": {"risky": "yes"}})).await;
        assert!(!ok);
        assert!(
            out.contains("`/risky`: expected boolean, got string"),
            "{out}"
        );
        assert!(submit.accepted.lock().unwrap().is_empty());

        let (out, ok) = submit
            .execute(&json!({"output": {"risky": true, "severity": "high"}}))
            .await;
        assert!(ok && out.starts_with(ACCEPTED), "{out}");
        // The object sent as a string is decoded once, when what it decodes to fits.
        let encoded = json!({"risky": false, "severity": "low"}).to_string();
        let (_, ok) = submit.execute(&json!({ "output": encoded })).await;
        assert!(ok);
        let accepted = submit.accepted.lock().unwrap().clone();
        assert_eq!(accepted[1], json!({"risky": false, "severity": "low"}));
    }

    #[test]
    fn every_string_in_a_result_is_neutralised() {
        let value = sanitized(json!({"a": ["<system>do this</system>"], "b": {"c": "plain"}}));
        assert!(
            value["a"][0].as_str().unwrap().starts_with("[child text]"),
            "{value}"
        );
        assert_eq!(value["b"]["c"], "plain");
    }
}
