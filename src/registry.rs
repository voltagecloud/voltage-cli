use serde::Deserialize;
use serde_json::Value;
use std::sync::LazyLock;

#[derive(Clone, Debug, Deserialize)]
pub struct Parameter {
    pub name: String,
    #[serde(rename = "in")]
    pub location: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub schema: Value,
    #[serde(default)]
    pub description: String,
    pub style: Option<String>,
    pub explode: Option<bool>,
}
#[derive(Clone, Debug, Deserialize)]
pub struct Operation {
    pub id: String,
    pub command: Vec<String>,
    pub target: Option<String>,
    pub method: String,
    pub path: String,
    pub auth: String,
    pub body: bool,
    pub description: String,
    pub parameters: Vec<Parameter>,
}
pub static OPERATIONS: LazyLock<Vec<Operation>> = LazyLock::new(|| {
    serde_json::from_str(include_str!(concat!(env!("OUT_DIR"), "/operations.json")))
        .expect("generated operation registry")
});
pub fn operation(id: &str) -> &'static Operation {
    OPERATIONS
        .iter()
        .find(|o| o.id == id)
        .expect("registered operation")
}
pub fn query_flag(name: &str) -> String {
    match name {
        "environment_id" | "environment_ids" => "env".into(),
        "wallet_id" => "wallet".into(),
        "line_of_credit_id" => "credit-line".into(),
        other => other.replace('_', "-"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_operation_has_a_unique_command() {
        let names: std::collections::BTreeSet<_> = OPERATIONS.iter().map(|o| &o.command).collect();
        assert_eq!(names.len(), OPERATIONS.len());
        assert_eq!(OPERATIONS.len(), 47);
    }
}
