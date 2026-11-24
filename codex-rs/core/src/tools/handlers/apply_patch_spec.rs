use codex_tools::FreeformTool;
use codex_tools::FreeformToolFormat;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use std::collections::BTreeMap;

const APPLY_PATCH_LARK_GRAMMAR: &str = include_str!("../../../assets/tools/apply_patch.lark");

/// Returns a custom tool that can be used to edit files. Well-suited for GPT-5 models
/// https://platform.openai.com/docs/guides/function-calling#custom-tools
pub fn create_apply_patch_freeform_tool(include_environment_id: bool) -> ToolSpec {
    let definition = if include_environment_id {
        APPLY_PATCH_LARK_GRAMMAR.replace(
            "start: begin_patch hunk+ end_patch",
            "start: begin_patch environment_id? hunk+ end_patch\nenvironment_id: \"*** Environment ID: \" filename LF",
        )
    } else {
        APPLY_PATCH_LARK_GRAMMAR.to_string()
    };
    ToolSpec::Freeform(FreeformTool {
        name: "apply_patch".to_string(),
        description: "The `apply_patch` tool can be used to edit files. This is a FREEFORM tool, so do not wrap the patch in JSON.".to_string(),
        defer_loading: None,
        format: FreeformToolFormat {
            r#type: "grammar".to_string(),
            syntax: "lark".to_string(),
            definition,
        },
    })
}

/// Returns a standard JSON `function` version of `apply_patch` for OpenAI-compatible local engines
/// (such as BaiInf `llama-server`) that only accept `"type": "function"` tools.
pub fn create_apply_patch_function_tool(include_environment_id: bool) -> ToolSpec {
    let mut properties = BTreeMap::from([(
        "input".to_string(),
        JsonSchema::string(Some(
            "Complete patch string starting with `*** Begin Patch` and ending with `*** End Patch` (`*** Update File: <path>`, `*** Add File: <path>`, or `*** Delete File: <path>`)."
                .to_string(),
        )),
    )]);
    if include_environment_id {
        properties.insert(
            "environment_id".to_string(),
            JsonSchema::string(Some(
                "Target environment ID when multiple execution environments are attached."
                    .to_string(),
            )),
        );
    }
    ToolSpec::Function(ResponsesApiTool {
        name: "apply_patch".to_string(),
        description: "Create, update, or delete files in the workspace using a structured `*** Begin Patch` ... `*** End Patch` diff passed in the `input` field.".to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(properties, Some(vec!["input".to_string()]), Some(false.into())),
        output_schema: None,
    })
}

#[cfg(test)]
#[path = "apply_patch_spec_tests.rs"]
mod tests;
