//! Generates the operation registry from the API snapshot and the explicit command mapping.
//!
//! The output feeds `registry::OPERATIONS`; its `auth`, `method`, and `target` strings must
//! deserialize into the closed enums in `src/registry.rs`. The build fails on a snapshot
//! operation without a mapping, a mapping without an operation, or a mapping whose target is
//! not a placeholder in the operation's path, so the two files cannot drift apart silently.

use serde_json::{Value, json};
use std::{collections::BTreeSet, env, fs, path::PathBuf};

const SNAPSHOT: &str = "api/openapi.json";
const MAPPING: &str = "api/commands.json";
const METHODS: [&str; 7] = ["get", "post", "put", "patch", "delete", "head", "options"];

fn read_json(path: &str) -> Value {
    let text =
        fs::read_to_string(path).unwrap_or_else(|error| panic!("Cannot read {path}: {error}"));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("Invalid JSON in {path}: {error}"))
}

fn requires_security(operation: &Value, scheme: &str) -> bool {
    operation["security"]
        .as_array()
        .is_some_and(|requirements| requirements.iter().any(|v| v.get(scheme).is_some()))
}

/// Closed string values a parameter accepts, resolved through `$ref`, nullable `oneOf`,
/// and array `items`; empty for free text.
fn allowed_values(spec: &Value, schema: &Value) -> Vec<String> {
    if let Some(reference) = schema["$ref"].as_str() {
        let name = reference.rsplit('/').next().unwrap_or_default();
        let component = &spec["components"]["schemas"][name];
        return component["enum"]
            .as_array()
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
    }
    if let Some(alternatives) = schema["oneOf"].as_array() {
        return alternatives
            .iter()
            .flat_map(|alternative| allowed_values(spec, alternative))
            .collect();
    }
    if schema.get("items").is_some() {
        return allowed_values(spec, &schema["items"]);
    }
    Vec::new()
}

fn auth_scheme(operation: &Value) -> &'static str {
    if requires_security(operation, "checkout_session_token") {
        "checkout_session"
    } else if requires_security(operation, "checkout_stream_token") {
        "checkout_stream"
    } else if requires_security(operation, "api_key") {
        "account"
    } else {
        "none"
    }
}

/// The mapping entry for one operation, checked for the shape the CLI relies on.
fn checked_mapping<'a>(commands: &'a Value, id: &str, path: &str) -> &'a Value {
    let mapping = commands
        .get(id)
        .unwrap_or_else(|| panic!("Unmapped API operation: {id}"));
    let words = mapping["command"]
        .as_array()
        .unwrap_or_else(|| panic!("Mapping for {id} needs a command word list"));
    assert!(
        !words.is_empty()
            && words.iter().all(|word| {
                word.as_str().is_some_and(|word| {
                    !word.is_empty() && word.bytes().all(|c| c.is_ascii_lowercase() || c == b'-')
                })
            }),
        "Mapping for {id} needs lowercase kebab-case command words"
    );
    match &mapping["target"] {
        Value::Null => {}
        Value::String(target) => assert!(
            path.contains(&format!("{{{target}}}")),
            "Mapping for {id} targets {target}, which is not a placeholder in {path}"
        ),
        _ => panic!("Mapping for {id} needs a null or string target"),
    }
    mapping
}

/// The registry record for one operation; parameters are the path item's and the
/// operation's, each annotated with its closed value set.
fn operation_record(
    spec: &Value,
    path: &str,
    method: &str,
    item: &Value,
    operation: &Value,
    mapping: &Value,
) -> Value {
    let id = operation["operationId"].as_str().expect("operationId");
    let mut parameters = item["parameters"].as_array().cloned().unwrap_or_default();
    parameters.extend(
        operation["parameters"]
            .as_array()
            .cloned()
            .unwrap_or_default(),
    );
    for parameter in &mut parameters {
        parameter["values"] = json!(allowed_values(spec, &parameter["schema"]));
    }
    json!({
        "id": id,
        "command": mapping["command"],
        "target": mapping["target"],
        "method": method.to_uppercase(),
        "path": path,
        "auth": auth_scheme(operation),
        "parameters": parameters,
        "body": operation.get("requestBody").is_some(),
        "description": operation.get("summary").and_then(Value::as_str).unwrap_or(id),
    })
}

fn main() {
    println!("cargo:rerun-if-changed={SNAPSHOT}");
    println!("cargo:rerun-if-changed={MAPPING}");
    let spec = read_json(SNAPSHOT);
    let commands = read_json(MAPPING);
    let mut operations = Vec::new();
    let mut seen = BTreeSet::new();
    for (path, item) in spec["paths"].as_object().expect("paths object") {
        for (method, operation) in item.as_object().expect("path item object") {
            if !METHODS.contains(&method.as_str()) {
                continue;
            }
            let id = operation["operationId"].as_str().expect("operationId");
            assert!(seen.insert(id.to_owned()), "Duplicate operation: {id}");
            let mapping = checked_mapping(&commands, id, path);
            operations.push(operation_record(
                &spec, path, method, item, operation, mapping,
            ));
        }
    }
    let mapped: BTreeSet<_> = commands
        .as_object()
        .expect("mapping object")
        .keys()
        .cloned()
        .collect();
    let stale: Vec<_> = mapped.difference(&seen).collect();
    assert!(stale.is_empty(), "Stale command mapping: {stale:?}");
    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR")).join("operations.json"),
        serde_json::to_vec(&operations).expect("operation registry"),
    )
    .expect("write operation registry");
}
