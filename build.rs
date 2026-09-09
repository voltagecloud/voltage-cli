use serde_json::{Value, json};
use std::{collections::BTreeSet, env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=api/openapi.json");
    println!("cargo:rerun-if-changed=api/commands.json");
    let spec: Value =
        serde_json::from_str(&fs::read_to_string("api/openapi.json").unwrap()).unwrap();
    let commands: Value =
        serde_json::from_str(&fs::read_to_string("api/commands.json").unwrap()).unwrap();
    let mut operations = Vec::new();
    let mut seen = BTreeSet::new();
    for (path, item) in spec["paths"].as_object().unwrap() {
        for (method, op) in item.as_object().unwrap() {
            if !["get", "post", "put", "patch", "delete", "head", "options"]
                .contains(&method.as_str())
            {
                continue;
            }
            let id = op["operationId"].as_str().expect("operationId");
            let mapping = commands
                .get(id)
                .unwrap_or_else(|| panic!("Unmapped API operation: {id}"));
            assert!(seen.insert(id.to_owned()), "Duplicate operation: {id}");
            let mut parameters = item["parameters"].as_array().cloned().unwrap_or_default();
            parameters.extend(op["parameters"].as_array().cloned().unwrap_or_default());
            let auth = if op["security"]
                .as_array()
                .is_some_and(|s| s.iter().any(|v| v.get("checkout_session_token").is_some()))
            {
                "checkout_session"
            } else if id == "get_events" {
                "checkout_stream"
            } else if op["security"]
                .as_array()
                .is_some_and(|s| s.iter().any(|v| v.get("api_key").is_some()))
            {
                "account"
            } else {
                "none"
            };
            operations.push(json!({"id":id,"command":mapping["command"],"target":mapping["target"],"method":method.to_uppercase(),"path":path,"auth":auth,"parameters":parameters,"body":op.get("requestBody").is_some(),"description":op.get("summary").and_then(Value::as_str).unwrap_or(id)}));
        }
    }
    assert_eq!(
        seen.len(),
        commands.as_object().unwrap().len(),
        "Stale command mapping"
    );
    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("operations.json"),
        serde_json::to_vec(&operations).unwrap(),
    )
    .unwrap();
}
