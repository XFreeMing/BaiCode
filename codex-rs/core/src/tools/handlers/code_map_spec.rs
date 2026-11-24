use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde_json::json;
use std::collections::BTreeMap;

pub fn create_code_map_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "mode".to_string(),
            JsonSchema::string_enum(
                vec![
                    json!("repo_map"),
                    json!("file_outline"),
                    json!("find_references"),
                ],
                Some(
                    "Operation mode: 'repo_map' (dependency-ranked workspace/directory symbol map, default), 'file_outline' (symbol outline of a single file), or 'find_references' (definitions and cross-file references for a symbol)."
                        .to_string(),
                ),
            ),
        ),
        (
            "path".to_string(),
            JsonSchema::string(Some(
                "Optional file or directory path relative to the workspace root (defaults to workspace root). Required for 'file_outline'."
                    .to_string(),
            )),
        ),
        (
            "symbol".to_string(),
            JsonSchema::string(Some(
                "Symbol name to search for. Required when mode is 'find_references'; optional focus boost when mode is 'repo_map'."
                    .to_string(),
            )),
        ),
        (
            "max_files".to_string(),
            JsonSchema::number(Some(
                "Maximum number of ranked files to include in 'repo_map' output (default 30, range 1..200)."
                    .to_string(),
            )),
        ),
        (
            "max_chars".to_string(),
            JsonSchema::number(Some(
                "Approximate character budget for the response (default 12000, range 1000..60000)."
                    .to_string(),
            )),
        ),
    ]);

    ToolSpec::Function(ResponsesApiTool {
        name: "code_map".to_string(),
        description: r#"Builds a compact structural map of the codebase without reading entire files.
Use this tool before broad shell searches to quickly understand repository layout, symbol definitions, and cross-file references:
- mode="repo_map": scans a directory (respecting .gitignore), extracts symbol definitions across Rust/Python/TS/JS/Go/C/C++/Java/Kotlin, ranks files by cross-file symbol references, and returns a compact skeleton.
- mode="file_outline": lists all top-level/class/function symbols and line numbers in a single file.
- mode="find_references": finds where a symbol is defined and where it is referenced across files."#
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(properties, None, Some(false.into())),
        output_schema: None,
    })
}
