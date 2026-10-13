// Documentation examples, shared by the unit test that parses them and the integration
// test that runs every documented change offline with `--dry-run`. Included with
// `include!`, so it names its own imports.

use std::path::PathBuf;

/// The README and the docs index, guides, concepts, reference, and development guide, as
/// (path from the repository root, contents).
pub fn user_docs() -> Vec<(PathBuf, String)> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut paths = ["README.md", "docs/README.md", "docs/development.md"]
        .map(|path| root.join(path))
        .to_vec();
    for dir in ["guides", "concepts", "reference"] {
        for entry in std::fs::read_dir(root.join("docs").join(dir)).unwrap() {
            paths.push(entry.unwrap().path());
        }
    }
    paths
        .into_iter()
        .map(|path| {
            let text = std::fs::read_to_string(&path).unwrap();
            (path.strip_prefix(&root).unwrap().to_owned(), text)
        })
        .collect()
}

/// Every `voltage` line in a documentation shell block, with continuations joined,
/// comments dropped, and pipes, redirections, and leading variable assignments cut away.
pub fn doc_examples() -> Vec<String> {
    let mut examples = Vec::new();
    for (_, text) in user_docs() {
        let mut shell = false;
        let mut line = String::new();
        for raw in text.lines().map(str::trim) {
            if let Some(language) = raw.strip_prefix("```") {
                shell = matches!(language, "sh" | "bash" | "shell" | "console");
                continue;
            }
            if !shell {
                continue;
            }
            line.push_str(raw.trim_end_matches('\\'));
            if raw.ends_with('\\') {
                continue;
            }
            let command = line.split(" #").next().unwrap_or_default();
            let command = command.split('|').find(|part| part.contains("voltage "));
            if let Some(command) = command.and_then(|part| part.split(['<', '>']).next()) {
                let start = command.find("voltage ").unwrap_or_default();
                let command = command[start..].trim();
                // `--build` belongs to the development guide's staging wrapper.
                if command != "voltage --build" {
                    examples.push(command.to_owned());
                }
            }
            line.clear();
        }
    }
    examples
}
