//! Jev-style System-1 typed decision-model tool router & trajectory recorder powered by local BaiDec.
//!
//! Before each generative sampling step on `BaiInf` (or any configured provider), this module can
//! query `BaiDec` (`POST /v1/decisions`, `Intern-Decision-4B`) with a compact structured `state`
//! and multi-head typed `questions` (`turn_action`, `primary_tool`, `code_map_mode`,
//! `needs_verification`). All questions share a single causal forward pass in BaiDec (~160 ms on
//! AI 300L) and return calibrated discrete decisions + probability distributions.
//!
//! Every step decision and its downstream execution outcome are optionally logged as structured
//! JSONL trajectories under `$BAICODE_HOME/baidec/trajectories/YYYY/MM/DD/` so BaiDec can be
//! continuously calibrated and fine-tuned (`Intern-Decision` SFT + `scope_replay` regression gate).

use std::collections::HashMap;
use std::collections::HashSet;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use chrono::Utc;
use codex_config::types::BaiDecConfig;
use codex_config::types::BaiDecMode;
use codex_http_client::ClientRouteClass;
use codex_http_client::HttpClientFactory;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::ResponseItem;
use codex_tools::ToolSpec;
use indexmap::IndexMap;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use tracing::debug;
use tracing::warn;
use uuid::Uuid;

use crate::client_common::Prompt;

/// Maximum candidates per `choice` field supported by BaiDec native engine (`BD_MAX_CANDIDATES_PER_FIELD = 62`).
/// Reserving 1 slot for `"none"` leaves 61 candidate tool slots.
const MAX_SHORTLIST_TOOLS: usize = 61;
const NONE_TOOL_KEY: &str = "none";
const CALL_TOOL_ACTION: &str = "call_tool";
const FINAL_ANSWER_ACTION: &str = "final_answer";
const RESERVED_DECISION_MARKER: &str = "<decision>";
const ESCAPED_DECISION_MARKER: &str = "< decision >";

pub fn sanitize_reserved_decision_marker(text: &str) -> String {
    if text.contains(RESERVED_DECISION_MARKER) {
        text.replace(RESERVED_DECISION_MARKER, ESCAPED_DECISION_MARKER)
    } else {
        text.to_string()
    }
}

fn sanitize_json_value(value: &mut Value) {
    match value {
        Value::String(s) => {
            if s.contains(RESERVED_DECISION_MARKER) {
                *s = s.replace(RESERVED_DECISION_MARKER, ESCAPED_DECISION_MARKER);
            }
        }
        Value::Array(items) => {
            for item in items {
                sanitize_json_value(item);
            }
        }
        Value::Object(map) => {
            let needs_key_rewrite = map
                .keys()
                .any(|k| k.contains(RESERVED_DECISION_MARKER));
            if needs_key_rewrite {
                let old = std::mem::take(map);
                for (k, mut v) in old {
                    sanitize_json_value(&mut v);
                    map.insert(sanitize_reserved_decision_marker(&k), v);
                }
            } else {
                for v in map.values_mut() {
                    sanitize_json_value(v);
                }
            }
        }
        _ => {}
    }
}

fn sanitize_baidec_request(request: &mut BaiDecRequest) {
    sanitize_json_value(&mut request.state);
    for question in request.questions.values_mut() {
        if let Some(instructions) = &mut question.instructions {
            *instructions = sanitize_reserved_decision_marker(instructions);
        }
        if let Some(criteria) = &mut question.criteria {
            for desc in criteria.values_mut() {
                *desc = sanitize_reserved_decision_marker(desc);
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolSemanticKind {
    CodeMap,
    LspMcp,
    EditPatch,
    ShellExec,
    StdinControl,
    Plan,
    UserInteraction,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateToolInfo {
    pub name: String,
    pub kind: ToolSemanticKind,
    pub criterion: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaiDecQuestion {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub criteria: Option<IndexMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaiDecRequest {
    pub state: Value,
    pub questions: IndexMap<String, BaiDecQuestion>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaiDecCalibration {
    pub method: Option<String>,
    pub temperature: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaiDecUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub decision_count: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaiDecAnswer {
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub decision: String,
    pub decision_index: Option<usize>,
    #[serde(default)]
    pub confidence: f64,
    pub choice: Option<String>,
    pub noul: Option<f64>,
    pub score: Option<f64>,
    #[serde(default)]
    pub probabilities: IndexMap<String, f64>,
    #[serde(default)]
    pub raw_probabilities: IndexMap<String, f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaiDecResponse {
    pub schema: Option<String>,
    pub model: Option<String>,
    pub backend: Option<String>,
    pub source: Option<String>,
    pub calibration: Option<BaiDecCalibration>,
    pub usage: Option<BaiDecUsage>,
    #[serde(default)]
    pub answers: IndexMap<String, BaiDecAnswer>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PolicyRoutingPlan {
    pub mode: BaiDecMode,
    pub applied: bool,
    pub turn_action: String,
    pub action_confidence: f64,
    pub primary_tool: String,
    pub primary_tool_confidence: f64,
    pub routed_tools: Vec<String>,
    pub tool_choice: String,
    pub code_map_mode_hint: Option<String>,
    pub needs_verification_prob: Option<f64>,
    pub degraded_reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ActiveDecisionStep {
    pub decision_id: String,
    pub step_index: usize,
    pub history_len_before: usize,
    pub plan: PolicyRoutingPlan,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservedToolCallOutcome {
    pub call_id: String,
    pub tool_name: String,
    pub arguments_preview: String,
    pub succeeded: bool,
    pub exit_code: Option<i32>,
    pub output_preview: String,
}

#[derive(Debug, Clone, Default)]
pub struct TurnWorkflowStage {
    pub code_map_completed: bool,
    pub patch_applied: bool,
    pub patch_retry_pending: bool,
    pub verification_passed: bool,
}

#[derive(Debug, Clone)]
struct CompiledPromptState {
    state: Value,
    step_index: usize,
    has_unverified_edit: bool,
    workflow: TurnWorkflowStage,
    last_failed_tool: Option<String>,
    user_goal: String,
}

/// Classify a model-visible tool into a coding-specific semantic category and concise criterion.
pub fn classify_candidate_tool(spec: &ToolSpec) -> Option<CandidateToolInfo> {
    let ToolSpec::Function(func) = spec else {
        return None;
    };
    let name = func.name.clone();
    let lower = name.to_ascii_lowercase();

    if name == "code_map" {
        return Some(CandidateToolInfo {
            name,
            kind: ToolSemanticKind::CodeMap,
            criterion: "Build structural code map before reading whole files or running broad grep: repo_map (ranked workspace/directory symbols), file_outline (symbols & line numbers of one file), or find_references (symbol definitions & cross-file callers)".to_string(),
        });
    }

    if is_lsp_mcp_tool_name(&lower, &func.description) {
        let summary = concise_description(&func.description, 180);
        return Some(CandidateToolInfo {
            name,
            kind: ToolSemanticKind::LspMcp,
            criterion: format!(
                "[LSP semantic tool] Compiler-accurate symbol navigation, references, hover types, or diagnostics: {summary}"
            ),
        });
    }

    if name == "apply_patch" {
        return Some(CandidateToolInfo {
            name,
            kind: ToolSemanticKind::EditPatch,
            criterion: "Create, modify, or delete source files with structured patch hunks once exact target file paths and context lines are known".to_string(),
        });
    }

    if matches!(name.as_str(), "exec_command" | "shell" | "local_shell") {
        return Some(CandidateToolInfo {
            name,
            kind: ToolSemanticKind::ShellExec,
            criterion: "Run shell commands in workspace: read specific line ranges, run cargo/pytest/npm build & unit tests, or inspect git status/diff".to_string(),
        });
    }

    if name == "write_stdin" {
        return Some(CandidateToolInfo {
            name,
            kind: ToolSemanticKind::StdinControl,
            criterion: "Write input to or poll output from an already running interactive command session".to_string(),
        });
    }

    if name == "update_plan" {
        return Some(CandidateToolInfo {
            name,
            kind: ToolSemanticKind::Plan,
            criterion: "Create or update checklist steps for multi-phase coding tasks".to_string(),
        });
    }

    if lower.contains("request_user_input") || lower.contains("send_message_to_user") {
        return Some(CandidateToolInfo {
            name,
            kind: ToolSemanticKind::UserInteraction,
            criterion: "Ask the user a brief clarifying question only when blocked by critical ambiguity".to_string(),
        });
    }

    Some(CandidateToolInfo {
        name,
        kind: ToolSemanticKind::Other,
        criterion: concise_description(&func.description, 200),
    })
}

fn is_lsp_mcp_tool_name(lower_name: &str, description: &str) -> bool {
    if !lower_name.starts_with("mcp__") {
        return false;
    }
    let lower_desc = description.to_ascii_lowercase();
    let lsp_server_or_op = [
        "lsp",
        "serena",
        "rust_analyzer",
        "rust-analyzer",
        "pyright",
        "gopls",
        "clangd",
        "tsserver",
        "jdtls",
        "definition",
        "references",
        "diagnostics",
        "hover",
        "document_symbol",
        "workspace_symbol",
        "find_symbol",
        "find_referencing",
        "call_hierarchy",
        "implementation",
    ];
    lsp_server_or_op
        .iter()
        .any(|kw| lower_name.contains(kw) || lower_desc.contains("language server"))
}

fn concise_description(raw: &str, max_chars: usize) -> String {
    let collapsed = raw
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    truncate_chars(&collapsed, max_chars)
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let head: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{head}…")
}

/// Deterministic shortlist capping candidate tools at 61 (`+ "none" = 62` BaiDec limit).
pub fn shortlist_candidate_tools(
    specs: &[ToolSpec],
    user_goal: &str,
) -> Vec<CandidateToolInfo> {
    let all_candidates: Vec<CandidateToolInfo> =
        specs.iter().filter_map(classify_candidate_tool).collect();
    if all_candidates.len() <= MAX_SHORTLIST_TOOLS {
        return all_candidates;
    }

    let goal_terms: HashSet<String> = user_goal
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|tok| tok.len() >= 3)
        .map(str::to_ascii_lowercase)
        .collect();

    let mut scored: Vec<(usize, i32, CandidateToolInfo)> = all_candidates
        .into_iter()
        .enumerate()
        .map(|(idx, info)| {
            let priority = match info.kind {
                ToolSemanticKind::CodeMap => 1000,
                ToolSemanticKind::LspMcp => 950,
                ToolSemanticKind::EditPatch => 900,
                ToolSemanticKind::ShellExec => 890,
                ToolSemanticKind::StdinControl => 850,
                ToolSemanticKind::Plan => 800,
                ToolSemanticKind::UserInteraction => 750,
                ToolSemanticKind::Other => {
                    let hay = format!("{} {}", info.name, info.criterion).to_ascii_lowercase();
                    let hits = goal_terms
                        .iter()
                        .filter(|term| hay.contains(term.as_str()))
                        .count() as i32;
                    100 + hits * 25
                }
            };
            (idx, priority, info)
        })
        .collect();

    scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    scored.truncate(MAX_SHORTLIST_TOOLS);
    // Restore original declaration order so prompt/schema ordering stays deterministic.
    scored.sort_by_key(|(idx, _, _)| *idx);
    scored.into_iter().map(|(_, _, info)| info).collect()
}

fn extract_message_text(content: &[ContentItem]) -> String {
    let mut parts = Vec::new();
    for item in content {
        match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text, .. } => {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    parts.push(trimmed);
                }
            }
            ContentItem::InputImage { .. } => parts.push("[image]"),
            _ => {}
        }
    }
    parts.join("\n")
}

fn is_verification_shell_command(arguments_json: &str) -> bool {
    let lower = arguments_json.to_ascii_lowercase();
    let verification_markers = [
        "cargo test",
        "cargo check",
        "cargo clippy",
        "cargo build",
        "pytest",
        "unittest",
        "npm test",
        "pnpm test",
        "yarn test",
        "go test",
        "ctest",
        "bazel test",
        "just test",
        "ruff check",
        "mypy",
        "tsc",
    ];
    verification_markers.iter().any(|kw| lower.contains(kw))
}

pub fn parse_tool_output_status_with_flag(
    output_text: &str,
    explicit_success: Option<bool>,
) -> (bool, Option<i32>) {
    for line in output_text.lines().take(8) {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Exit code:")
            && let Ok(code) = rest.trim().parse::<i32>()
        {
            return (code == 0 && explicit_success != Some(false), Some(code));
        }
        if let Some(rest) = trimmed.strip_prefix("Process exited with code ")
            && let Ok(code) = rest.trim().parse::<i32>()
        {
            return (code == 0 && explicit_success != Some(false), Some(code));
        }
    }
    if explicit_success == Some(false) {
        return (false, None);
    }
    let lower = output_text.to_ascii_lowercase();
    let looks_failed = lower.starts_with("error:")
        || lower.contains("apply_patch verification failed")
        || lower.contains("failed to parse function arguments")
        || lower.contains("failed to parse apply_patch")
        || lower.contains("failed to parse code_map")
        || lower.contains("command timed out")
        || lower.contains("permission denied")
        || lower.contains("approval policy is never")
        || lower.contains("reject command")
        || lower.contains("rejected by user")
        || lower.contains("sandbox denied");
    (!looks_failed, None)
}

fn compile_prompt_state(
    input: &[ResponseItem],
    candidates: &[CandidateToolInfo],
    cwd: &Path,
    model_slug: &str,
    max_state_chars: usize,
) -> CompiledPromptState {
    // Locate the start of the active user turn (last non-contextual user message).
    let mut last_user_idx = None;
    let mut user_goal = String::new();
    for (idx, item) in input.iter().enumerate().rev() {
        if let ResponseItem::Message { role, content, .. } = item
            && role == "user"
        {
            let text = extract_message_text(content);
            if !text.is_empty()
                && !text.starts_with("<environment_context>")
                && !text.starts_with("<permissions instructions>")
                && !text.starts_with("<collaboration_mode>")
                && !text.starts_with("# BAICODE.md")
                && !text.starts_with("# AGENTS.md")
            {
                last_user_idx = Some(idx);
                user_goal = truncate_chars(&text, max_state_chars / 3);
                break;
            }
        }
    }

    let slice_start = last_user_idx.unwrap_or(0);
    let mut call_names_by_id: HashMap<String, (String, String)> = HashMap::new();
    let mut step_index = 0usize;
    let mut has_unverified_edit = false;
    let mut workflow = TurnWorkflowStage::default();
    let mut last_failed_tool: Option<String> = None;
    let mut recent_events: Vec<Value> = Vec::new();

    for item in &input[slice_start..] {
        match item {
            ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            } => {
                step_index += 1;
                let args_preview = truncate_chars(arguments.trim(), 160);
                call_names_by_id.insert(call_id.clone(), (name.clone(), args_preview.clone()));
                recent_events.push(json!({
                    "event": "tool_call",
                    "tool": name,
                    "args": args_preview,
                }));
            }
            ResponseItem::CustomToolCall {
                name,
                input: raw_input,
                call_id,
                ..
            } => {
                step_index += 1;
                let args_preview = truncate_chars(raw_input.trim(), 160);
                call_names_by_id.insert(call_id.clone(), (name.clone(), args_preview.clone()));
                recent_events.push(json!({
                    "event": "tool_call",
                    "tool": name,
                    "args": args_preview,
                }));
            }
            ResponseItem::FunctionCallOutput {
                call_id, output, ..
            } => {
                let raw_text = match &output.body {
                    FunctionCallOutputBody::Text(text) => text.as_str(),
                    FunctionCallOutputBody::ContentItems(_) => "[structured tool output]",
                };
                let (ok, exit_code) = parse_tool_output_status_with_flag(raw_text, output.success);
                let (tool_name, args_preview) = call_id
                    .as_deref()
                    .and_then(|id| call_names_by_id.get(id))
                    .cloned()
                    .unwrap_or_else(|| ("unknown".to_string(), String::new()));

                if tool_name == "code_map" && ok {
                    workflow.code_map_completed = true;
                } else if tool_name == "apply_patch" {
                    if ok {
                        has_unverified_edit = true;
                        workflow.patch_applied = true;
                        workflow.patch_retry_pending = false;
                        workflow.verification_passed = false;
                    } else {
                        workflow.patch_retry_pending = true;
                    }
                } else if matches!(tool_name.as_str(), "exec_command" | "shell" | "local_shell")
                    && ok
                    && is_verification_shell_command(&args_preview)
                {
                    has_unverified_edit = false;
                    workflow.verification_passed = true;
                }

                if ok {
                    last_failed_tool = None;
                } else {
                    last_failed_tool = Some(tool_name.clone());
                }

                recent_events.push(json!({
                    "event": "tool_result",
                    "tool": tool_name,
                    "ok": ok,
                    "exit_code": exit_code,
                    "preview": truncate_chars(raw_text.trim(), 200),
                }));
            }
            ResponseItem::CustomToolCallOutput {
                call_id, output, ..
            } => {
                let raw_text = match &output.body {
                    FunctionCallOutputBody::Text(text) => text.as_str(),
                    FunctionCallOutputBody::ContentItems(_) => "[structured tool output]",
                };
                let (ok, exit_code) = parse_tool_output_status_with_flag(raw_text, output.success);
                let (tool_name, args_preview) = call_names_by_id
                    .get(call_id)
                    .cloned()
                    .unwrap_or_else(|| ("unknown".to_string(), String::new()));

                if tool_name == "apply_patch" {
                    if ok {
                        has_unverified_edit = true;
                        workflow.patch_applied = true;
                        workflow.patch_retry_pending = false;
                        workflow.verification_passed = false;
                    } else {
                        workflow.patch_retry_pending = true;
                    }
                } else if matches!(tool_name.as_str(), "exec_command" | "shell" | "local_shell")
                    && ok
                    && is_verification_shell_command(&args_preview)
                {
                    has_unverified_edit = false;
                    workflow.verification_passed = true;
                }

                if ok {
                    last_failed_tool = None;
                } else {
                    last_failed_tool = Some(tool_name.clone());
                }

                recent_events.push(json!({
                    "event": "tool_result",
                    "tool": tool_name,
                    "ok": ok,
                    "exit_code": exit_code,
                    "preview": truncate_chars(raw_text.trim(), 200),
                }));
            }
            ResponseItem::Message { role, content, .. } if role == "assistant" => {
                let text = extract_message_text(content);
                if !text.is_empty() {
                    recent_events.push(json!({
                        "event": "assistant_note",
                        "text": truncate_chars(&text, 160),
                    }));
                }
            }
            _ => {}
        }
    }

    // Retain at most the last 5 events so `state` stays crisp and fast (~500ms on BaiDec).
    if recent_events.len() > 5 {
        let drain_count = recent_events.len() - 5;
        recent_events.drain(0..drain_count);
    }

    let has_code_map = candidates
        .iter()
        .any(|c| c.kind == ToolSemanticKind::CodeMap);
    let lsp_tools: Vec<String> = candidates
        .iter()
        .filter(|c| c.kind == ToolSemanticKind::LspMcp)
        .map(|c| c.name.clone())
        .collect();
    let tool_summaries: IndexMap<String, String> = candidates
        .iter()
        .map(|c| (c.name.clone(), truncate_chars(&c.criterion, 100)))
        .collect();

    let mut state = json!({
        "workspace": cwd.display().to_string(),
        "generator_model": model_slug,
        "step_index": step_index,
        "workflow_progress": {
            "code_map_completed": workflow.code_map_completed,
            "patch_applied": workflow.patch_applied,
            "patch_retry_pending": workflow.patch_retry_pending,
            "has_unverified_edit": has_unverified_edit,
            "verification_passed": workflow.verification_passed,
        },
        "last_failed_tool": last_failed_tool,
        "navigation_capabilities": {
            "builtin_code_map": has_code_map,
            "lsp_mcp_tools": lsp_tools,
        },
        "user_goal": user_goal,
        "recent_steps": recent_events,
        "available_tools": tool_summaries,
    });

    if let Ok(serialized) = serde_json::to_string(&state)
        && serialized.len() > max_state_chars
    {
        // Gracefully trim recent_steps to stay strictly under `max_state_chars`.
        if let Some(obj) = state.as_object_mut() {
            let trimmed_goal = truncate_chars(&user_goal, max_state_chars / 4);
            obj.insert("user_goal".to_string(), json!(trimmed_goal));
            let keep = recent_events.len().min(3);
            let start = recent_events.len().saturating_sub(keep);
            obj.insert(
                "recent_steps".to_string(),
                json!(recent_events[start..].to_vec()),
            );
        }
    }

    CompiledPromptState {
        state,
        step_index,
        has_unverified_edit,
        workflow,
        last_failed_tool,
        user_goal,
    }
}

pub fn build_decision_questions(
    candidates: &[CandidateToolInfo],
) -> IndexMap<String, BaiDecQuestion> {
    let mut questions = IndexMap::new();

    let mut action_criteria = IndexMap::new();
    action_criteria.insert(
        CALL_TOOL_ACTION.to_string(),
        "Invoke a tool in this step to map code structure, inspect files/symbols, edit files with apply_patch, or run shell build/test commands".to_string(),
    );
    action_criteria.insert(
        FINAL_ANSWER_ACTION.to_string(),
        "All required inspections, edits, and verifications are already satisfied (or user asked a pure conceptual question); write the final response directly without calling tools".to_string(),
    );
    questions.insert(
        "turn_action".to_string(),
        BaiDecQuestion {
            kind: "choice".to_string(),
            instructions: Some(
                "Decide whether the coding agent should call a tool right now or finish the turn with a direct final answer.".to_string(),
            ),
            criteria: Some(action_criteria),
        },
    );

    let mut tool_criteria = IndexMap::new();
    for candidate in candidates {
        tool_criteria.insert(candidate.name.clone(), candidate.criterion.clone());
    }
    tool_criteria.insert(
        NONE_TOOL_KEY.to_string(),
        "No tool needed; respond directly to the user".to_string(),
    );
    questions.insert(
        "primary_tool".to_string(),
        BaiDecQuestion {
            kind: "choice".to_string(),
            instructions: Some(
                "Select the best next tool for this coding step. Prefer code_map or LSP MCP tools for repository/symbol discovery before broad shell searches, apply_patch for file edits, exec_command for targeted line reads and build/test verification, or none when ready to answer.".to_string(),
            ),
            criteria: Some(tool_criteria),
        },
    );

    if candidates.iter().any(|c| c.kind == ToolSemanticKind::CodeMap) {
        let mut cm_criteria = IndexMap::new();
        cm_criteria.insert(
            "repo_map".to_string(),
            "Overview of workspace or directory architecture and dependency-ranked key files"
                .to_string(),
        );
        cm_criteria.insert(
            "file_outline".to_string(),
            "List structs, classes, functions, and exact line numbers inside a single known file"
                .to_string(),
        );
        cm_criteria.insert(
            "find_references".to_string(),
            "Find where a specific symbol is defined and referenced across the repository"
                .to_string(),
        );
        questions.insert(
            "code_map_mode".to_string(),
            BaiDecQuestion {
                kind: "choice".to_string(),
                instructions: Some(
                    "If code_map is used on this step, which mode best fits the immediate code navigation need?".to_string(),
                ),
                criteria: Some(cm_criteria),
            },
        );
    }

    questions.insert(
        "needs_verification".to_string(),
        BaiDecQuestion {
            kind: "noul".to_string(),
            instructions: Some(
                "Have source files been modified or does the user goal still require running a build/test verification command before concluding?".to_string(),
            ),
            criteria: None,
        },
    );

    questions
}

/// Evaluate BaiDec's calibrated answers into an actionable tool-routing plan with safety guardrails.
pub fn evaluate_routing_decision(
    cfg: &BaiDecConfig,
    candidates: &[CandidateToolInfo],
    step_index: usize,
    has_unverified_edit: bool,
    workflow: &TurnWorkflowStage,
    last_failed_tool: Option<&str>,
    response: &BaiDecResponse,
) -> PolicyRoutingPlan {
    let candidate_names: HashSet<&str> = candidates.iter().map(|c| c.name.as_str()).collect();

    let Some(action_ans) = response.answers.get("turn_action") else {
        return degraded_fallback(cfg.mode, candidates, "missing_turn_action_answer");
    };
    let Some(tool_ans) = response.answers.get("primary_tool") else {
        return degraded_fallback(cfg.mode, candidates, "missing_primary_tool_answer");
    };

    let turn_action = action_ans
        .choice
        .as_deref()
        .unwrap_or(action_ans.decision.as_str())
        .to_string();
    let action_confidence = action_ans.confidence;

    let raw_primary_tool = tool_ans
        .choice
        .as_deref()
        .unwrap_or(tool_ans.decision.as_str())
        .to_string();
    let primary_tool_confidence = tool_ans.confidence;

    if raw_primary_tool != NONE_TOOL_KEY && !candidate_names.contains(raw_primary_tool.as_str()) {
        return degraded_fallback(cfg.mode, candidates, "unknown_candidate_tool");
    }

    let needs_verification_prob = response
        .answers
        .get("needs_verification")
        .and_then(|ans| ans.noul.or_else(|| ans.probabilities.get("yes").copied()));

    let code_map_mode_hint = response
        .answers
        .get("code_map_mode")
        .map(|ans| {
            ans.choice
                .as_deref()
                .unwrap_or(ans.decision.as_str())
                .to_string()
        })
        .filter(|m| matches!(m.as_str(), "repo_map" | "file_outline" | "find_references"));

    // Guardrail 1: Check whether BaiDec confidently agrees that no tool is needed (`final_answer` + `none`).
    let unverified_guard_blocks_finish = has_unverified_edit
        && candidate_names.contains("exec_command")
        && needs_verification_prob.unwrap_or(1.0) >= 0.55
        && action_confidence < 0.88;

    let wants_final_answer = turn_action == FINAL_ANSWER_ACTION
        && raw_primary_tool == NONE_TOOL_KEY
        && action_confidence >= cfg.min_action_confidence
        && !unverified_guard_blocks_finish;

    if wants_final_answer {
        let enforce = cfg.mode == BaiDecMode::Enforce;
        return PolicyRoutingPlan {
            mode: cfg.mode,
            applied: enforce,
            turn_action,
            action_confidence,
            primary_tool: NONE_TOOL_KEY.to_string(),
            primary_tool_confidence,
            routed_tools: Vec::new(),
            tool_choice: if enforce {
                "none".to_string()
            } else {
                "auto".to_string()
            },
            code_map_mode_hint: None,
            needs_verification_prob,
            degraded_reason: None,
        };
    }

    // Guardrail 2: Disagreement between `turn_action` and `primary_tool` (e.g. `call_tool` + `none`
    // or `final_answer` + concrete tool on step 0). Rank non-`none` tools by calibrated probability.
    let mut ranked_tools: Vec<(String, f64)> = candidates
        .iter()
        .map(|c| {
            let prob = tool_ans
                .probabilities
                .get(&c.name)
                .copied()
                .unwrap_or(0.0);
            (c.name.clone(), prob)
        })
        .collect();
    ranked_tools.sort_by(|a, b| b.1.total_cmp(&a.1));

    let (effective_primary, effective_primary_prob) = if raw_primary_tool != NONE_TOOL_KEY {
        let prob = tool_ans
            .probabilities
            .get(&raw_primary_tool)
            .copied()
            .unwrap_or(primary_tool_confidence);
        (raw_primary_tool.clone(), prob)
    } else if unverified_guard_blocks_finish && candidate_names.contains("exec_command") {
        let prob = tool_ans
            .probabilities
            .get("exec_command")
            .copied()
            .unwrap_or(0.5);
        ("exec_command".to_string(), prob)
    } else if let Some((top_name, top_prob)) = ranked_tools.first() {
        (top_name.clone(), *top_prob)
    } else {
        return degraded_fallback(cfg.mode, candidates, "no_candidate_tools");
    };

    let mut routed_tools = vec![effective_primary.clone()];
    for (name, prob) in &ranked_tools {
        if routed_tools.len() >= cfg.max_routed_tools {
            break;
        }
        if name != &effective_primary && *prob >= cfg.secondary_tool_threshold {
            routed_tools.push(name.clone());
        }
    }

    // Sticky recovery: if an earlier `apply_patch` failed and has not yet succeeded, keep both
    // `apply_patch` and `exec_command` in the routed set so the model can inspect context & re-apply!
    if workflow.patch_retry_pending {
        for needed in ["apply_patch", "exec_command"] {
            if candidate_names.contains(needed) && !routed_tools.iter().any(|t| t == needed) {
                if routed_tools.len() >= cfg.max_routed_tools.max(3) {
                    routed_tools.pop();
                }
                routed_tools.push(needed.to_string());
            }
        }
    }

    // Once `code_map` has already run and edits haven't been applied yet, ensure `apply_patch`
    // and `exec_command` are not starved out if zero-shot BaiDec stays anchored on `code_map` in user_goal!
    if workflow.code_map_completed && !workflow.patch_applied && step_index >= 2 {
        for needed in ["exec_command", "apply_patch"] {
            if candidate_names.contains(needed)
                && !routed_tools.iter().any(|t| t == needed)
                && routed_tools.len() < cfg.max_routed_tools.max(3)
            {
                routed_tools.push(needed.to_string());
            }
        }
    }

    // Anti-loop recovery: if the chosen primary tool just failed on the immediately preceding step,
    // keep `code_map` or `exec_command` available as a recovery fallback within `max_routed_tools`.
    if last_failed_tool == Some(effective_primary.as_str()) {
        for fallback in ["code_map", "exec_command"] {
            if routed_tools.len() >= cfg.max_routed_tools.max(2) {
                break;
            }
            if candidate_names.contains(fallback) && !routed_tools.iter().any(|t| t == fallback) {
                routed_tools.push(fallback.to_string());
            }
        }
    }

    // Disagreement or post-verification completion uses `tool_choice = "auto"` over the narrowed
    // shortlist so Qwen can cleanly conclude with its final answer as soon as tests pass!
    let head_disagreement =
        (turn_action == FINAL_ANSWER_ACTION) != (raw_primary_tool == NONE_TOOL_KEY);
    let confident_tool_call = !head_disagreement
        && !workflow.verification_passed
        && effective_primary_prob >= cfg.min_tool_confidence
        && last_failed_tool != Some(effective_primary.as_str());

    let chosen_tool_choice = if cfg.mode != BaiDecMode::Enforce {
        "auto".to_string()
    } else if workflow.verification_passed {
        "auto".to_string()
    } else if has_unverified_edit || workflow.patch_retry_pending {
        "required".to_string()
    } else if step_index == 0
        && turn_action == CALL_TOOL_ACTION
        && effective_primary_prob >= cfg.min_tool_confidence * 0.8
    {
        "required".to_string()
    } else if confident_tool_call && action_confidence >= 0.85 && step_index <= 3 {
        "required".to_string()
    } else {
        "auto".to_string()
    };

    let cm_hint = if routed_tools.iter().any(|t| t == "code_map") {
        code_map_mode_hint
    } else {
        None
    };

    PolicyRoutingPlan {
        mode: cfg.mode,
        applied: cfg.mode == BaiDecMode::Enforce,
        turn_action,
        action_confidence,
        primary_tool: effective_primary,
        primary_tool_confidence: effective_primary_prob,
        routed_tools,
        tool_choice: chosen_tool_choice,
        code_map_mode_hint: cm_hint,
        needs_verification_prob,
        degraded_reason: None,
    }
}

fn degraded_fallback(
    mode: BaiDecMode,
    candidates: &[CandidateToolInfo],
    reason: &str,
) -> PolicyRoutingPlan {
    PolicyRoutingPlan {
        mode,
        applied: false,
        turn_action: CALL_TOOL_ACTION.to_string(),
        action_confidence: 0.0,
        primary_tool: NONE_TOOL_KEY.to_string(),
        primary_tool_confidence: 0.0,
        routed_tools: candidates.iter().map(|c| c.name.clone()).collect(),
        tool_choice: "auto".to_string(),
        code_map_mode_hint: None,
        needs_verification_prob: None,
        degraded_reason: Some(reason.to_string()),
    }
}

fn build_ephemeral_routing_hint(plan: &PolicyRoutingPlan) -> ResponseItem {
    let routed_list = plan.routed_tools.join(", ");
    let mode_clause = match (&plan.code_map_mode_hint, plan.primary_tool.as_str()) {
        (Some(cm_mode), "code_map") => {
            format!(" Recommended `code_map` mode: `\"{cm_mode}\"` (`repo_map` for workspace overview, `file_outline` for a single file's symbol lines, `find_references` for cross-file callers/definitions).")
        }
        _ => String::new(),
    };
    let text = format!(
        "<baidec_decision mode=\"enforce\" primary_tool=\"{}\" confidence=\"{:.3}\" routed_tools=\"{}\" tool_choice=\"{}\">BaiDec decision router selected `{}` for this step.{mode_clause} Focus strictly on producing valid JSON arguments grounded in real workspace paths and symbols.</baidec_decision>",
        plan.primary_tool,
        plan.primary_tool_confidence,
        routed_list,
        plan.tool_choice,
        plan.primary_tool,
    );
    ResponseItem::Message {
        id: None,
        role: "developer".to_string(),
        content: vec![ContentItem::InputText { text }],
        encrypted_content: None,
        status: None,
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

async fn query_baidec_endpoint(
    http_client_factory: &HttpClientFactory,
    cfg: &BaiDecConfig,
    request: &BaiDecRequest,
) -> Result<BaiDecResponse, String> {
    let client = http_client_factory
        .build_client(&cfg.url, ClientRouteClass::Other)
        .map_err(|err| format!("build_client_failed: {err}"))?;
    let response = client
        .post(&cfg.url)
        .timeout(Duration::from_millis(cfg.timeout_ms))
        .json(request)
        .send()
        .await
        .map_err(|err| format!("http_send_failed: {err}"))?;

    let status = response.status();
    if !status.is_success() {
        let body_preview = response
            .text()
            .await
            .unwrap_or_default()
            .chars()
            .take(240)
            .collect::<String>();
        return Err(format!("http_{}: {body_preview}", status.as_u16()));
    }

    response
        .json::<BaiDecResponse>()
        .await
        .map_err(|err| format!("invalid_json: {err}"))
}

/// Apply BaiDec Jev-style decision routing to a prepared `Prompt` prior to generative sampling.
///
/// Never mutates persistent session history; any routing directive or narrowed `tools` slice lives
/// solely on this step's `Prompt`. On any BaiDec failure, falls back cleanly to canonical tools.
#[allow(clippy::too_many_arguments)]
pub async fn maybe_route_prompt_tools(
    cfg: &BaiDecConfig,
    http_client_factory: &HttpClientFactory,
    thread_id: &str,
    turn_id: &str,
    cwd: &Path,
    model_slug: &str,
    history_len_before: usize,
    prompt: &mut Prompt,
) -> Option<ActiveDecisionStep> {
    if !cfg.mode.is_active() || prompt.tools.is_empty() {
        return None;
    }

    // Extract user goal preview first for shortlisting if >61 tools are registered.
    let quick_goal = prompt
        .input
        .iter()
        .rev()
        .find_map(|item| match item {
            ResponseItem::Message { role, content, .. } if role == "user" => {
                let text = extract_message_text(content);
                (!text.is_empty() && !text.starts_with('<')).then_some(text)
            }
            _ => None,
        })
        .unwrap_or_default();

    let candidates = shortlist_candidate_tools(&prompt.tools, &quick_goal);
    if candidates.is_empty() {
        return None;
    }

    let compiled = compile_prompt_state(
        &prompt.input,
        &candidates,
        cwd,
        model_slug,
        cfg.max_state_chars,
    );
    let questions = build_decision_questions(&candidates);
    let mut request = BaiDecRequest {
        state: compiled.state,
        questions,
    };
    sanitize_baidec_request(&mut request);

    let decision_id = Uuid::now_v7().to_string();
    let started = Instant::now();
    let rpc_result = query_baidec_endpoint(http_client_factory, cfg, &request).await;
    let latency_ms = started.elapsed().as_secs_f64() * 1000.0;

    let (plan, raw_response) = match rpc_result {
        Ok(resp) => {
            let plan = evaluate_routing_decision(
                cfg,
                &candidates,
                compiled.step_index,
                compiled.has_unverified_edit,
                &compiled.workflow,
                compiled.last_failed_tool.as_deref(),
                &resp,
            );
            debug!(
                decision_id = %decision_id,
                mode = %cfg.mode.as_str(),
                step_index = compiled.step_index,
                turn_action = %plan.turn_action,
                primary_tool = %plan.primary_tool,
                confidence = plan.primary_tool_confidence,
                routed_tools = ?plan.routed_tools,
                tool_choice = %plan.tool_choice,
                latency_ms,
                "BaiDec tool routing decision completed"
            );
            (plan, Some(resp))
        }
        Err(err) => {
            warn!(
                decision_id = %decision_id,
                url = %cfg.url,
                latency_ms,
                error = %err,
                "BaiDec decision request degraded; falling back to canonical auto tool routing"
            );
            (
                degraded_fallback(cfg.mode, &candidates, &err),
                None,
            )
        }
    };

    if plan.applied {
        if plan.tool_choice == "none" {
            prompt.tools = Arc::default();
            prompt.tool_choice = Some("none".to_string());
        } else {
            let allowed: HashSet<&str> = plan.routed_tools.iter().map(String::as_str).collect();
            let filtered: Vec<ToolSpec> = prompt
                .tools
                .iter()
                .filter(|spec| match spec {
                    ToolSpec::Function(func) => allowed.contains(func.name.as_str()),
                    ToolSpec::WebSearch { .. } => true,
                    _ => false,
                })
                .cloned()
                .collect();
            if !filtered.is_empty() {
                if filtered.len() <= 1 {
                    prompt.parallel_tool_calls = false;
                }
                prompt.tools = filtered.into();
                prompt.tool_choice = Some(plan.tool_choice.clone());
                prompt.input.push(build_ephemeral_routing_hint(&plan));
            }
        }
    }

    if cfg.record_trajectories {
        let record = json!({
            "schema": "baicode-baidec-trajectory-v1",
            "kind": "decision_step",
            "timestamp": Utc::now().to_rfc3339(),
            "thread_id": thread_id,
            "turn_id": turn_id,
            "decision_id": decision_id,
            "step_index": compiled.step_index,
            "generator_model": model_slug,
            "user_goal": compiled.user_goal,
            "has_unverified_edit": compiled.has_unverified_edit,
            "latency_ms": (latency_ms * 100.0).round() / 100.0,
            "candidates": candidates,
            "request": request,
            "response": raw_response,
            "routing_plan": plan,
        });
        append_trajectory_record(&cfg.trajectory_dir, thread_id, &record);
    }

    Some(ActiveDecisionStep {
        decision_id,
        step_index: compiled.step_index,
        history_len_before,
        plan,
    })
}

/// Record downstream tool call execution & assistant output outcomes for an active `BaiDec` step.
///
/// Joining `decision_step` + `step_outcome` gives offline fine-tuning (`baidec_flywheel.py`) exact
/// ground-truth labels, execution success rewards, and counterfactual repair signals.
#[allow(clippy::too_many_arguments)]
pub fn record_step_outcome(
    cfg: &BaiDecConfig,
    thread_id: &str,
    turn_id: &str,
    active_step: &ActiveDecisionStep,
    history_items_after: &[ResponseItem],
    needs_follow_up: bool,
    sampling_error: Option<&str>,
) {
    if !cfg.mode.is_active() || !cfg.record_trajectories {
        return;
    }

    let start = active_step
        .history_len_before
        .min(history_items_after.len());
    let delta_items = &history_items_after[start..];

    let mut calls_by_id: HashMap<String, (String, String)> = HashMap::new();
    let mut call_order: Vec<String> = Vec::new();
    let mut outcomes: Vec<ObservedToolCallOutcome> = Vec::new();
    let mut produced_assistant_message = false;

    for item in delta_items {
        match item {
            ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            } => {
                calls_by_id.insert(
                    call_id.clone(),
                    (name.clone(), truncate_chars(arguments.trim(), 300)),
                );
                call_order.push(call_id.clone());
            }
            ResponseItem::CustomToolCall {
                name,
                input,
                call_id,
                ..
            } => {
                calls_by_id.insert(
                    call_id.clone(),
                    (name.clone(), truncate_chars(input.trim(), 300)),
                );
                call_order.push(call_id.clone());
            }
            ResponseItem::FunctionCallOutput {
                call_id, output, ..
            } => {
                let raw_text = match &output.body {
                    FunctionCallOutputBody::Text(text) => text.as_str(),
                    FunctionCallOutputBody::ContentItems(_) => "[structured tool output]",
                };
                let (succeeded, exit_code) =
                    parse_tool_output_status_with_flag(raw_text, output.success);
                let resolved_id = call_id.clone().unwrap_or_default();
                let (tool_name, arguments_preview) = calls_by_id
                    .remove(&resolved_id)
                    .unwrap_or_else(|| ("unknown".to_string(), String::new()));
                outcomes.push(ObservedToolCallOutcome {
                    call_id: resolved_id,
                    tool_name,
                    arguments_preview,
                    succeeded,
                    exit_code,
                    output_preview: truncate_chars(raw_text.trim(), 300),
                });
            }
            ResponseItem::CustomToolCallOutput {
                call_id, output, ..
            } => {
                let raw_text = match &output.body {
                    FunctionCallOutputBody::Text(text) => text.as_str(),
                    FunctionCallOutputBody::ContentItems(_) => "[structured tool output]",
                };
                let (succeeded, exit_code) =
                    parse_tool_output_status_with_flag(raw_text, output.success);
                let (tool_name, arguments_preview) = calls_by_id
                    .remove(call_id)
                    .unwrap_or_else(|| ("unknown".to_string(), String::new()));
                outcomes.push(ObservedToolCallOutcome {
                    call_id: call_id.clone(),
                    tool_name,
                    arguments_preview,
                    succeeded,
                    exit_code,
                    output_preview: truncate_chars(raw_text.trim(), 300),
                });
            }
            ResponseItem::Message { role, content, .. } if role == "assistant" => {
                if !extract_message_text(content).is_empty() {
                    produced_assistant_message = true;
                }
            }
            _ => {}
        }
    }

    // Include any calls that did not emit a matching output item (e.g. interrupted).
    for call_id in call_order {
        if let Some((tool_name, arguments_preview)) = calls_by_id.remove(&call_id) {
            outcomes.push(ObservedToolCallOutcome {
                call_id,
                tool_name,
                arguments_preview,
                succeeded: false,
                exit_code: None,
                output_preview: "[no output recorded]".to_string(),
            });
        }
    }

    let all_tools_succeeded = !outcomes.is_empty() && outcomes.iter().all(|o| o.succeeded);
    let actual_action = if !outcomes.is_empty() {
        CALL_TOOL_ACTION
    } else if produced_assistant_message && !needs_follow_up {
        FINAL_ANSWER_ACTION
    } else {
        "incomplete"
    };
    let actual_primary_tool = outcomes
        .first()
        .map(|o| o.tool_name.as_str())
        .unwrap_or(NONE_TOOL_KEY);

    let record = json!({
        "schema": "baicode-baidec-trajectory-v1",
        "kind": "step_outcome",
        "timestamp": Utc::now().to_rfc3339(),
        "thread_id": thread_id,
        "turn_id": turn_id,
        "decision_id": active_step.decision_id,
        "step_index": active_step.step_index,
        "baidec_predicted_action": active_step.plan.turn_action,
        "baidec_predicted_tool": active_step.plan.primary_tool,
        "actual_action": actual_action,
        "actual_primary_tool": actual_primary_tool,
        "action_agreed": active_step.plan.turn_action == actual_action,
        "tool_agreed": active_step.plan.primary_tool == actual_primary_tool,
        "all_tools_succeeded": all_tools_succeeded,
        "produced_final_answer": produced_assistant_message && !needs_follow_up,
        "needs_follow_up": needs_follow_up,
        "sampling_error": sampling_error,
        "observed_tool_calls": outcomes,
    });
    append_trajectory_record(&cfg.trajectory_dir, thread_id, &record);
}

fn trajectory_file_path(base_dir: &Path, thread_id: &str) -> PathBuf {
    let now = Utc::now();
    let date_dir = base_dir
        .join(now.format("%Y").to_string())
        .join(now.format("%m").to_string())
        .join(now.format("%d").to_string());
    let sanitized_thread: String = thread_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    date_dir.join(format!("decisions-{sanitized_thread}.jsonl"))
}

fn append_trajectory_record(base_dir: &Path, thread_id: &str, record: &Value) {
    if base_dir.as_os_str().is_empty() {
        return;
    }
    let path = trajectory_file_path(base_dir, thread_id);
    if let Some(parent) = path.parent()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        warn!(
            path = %parent.display(),
            error = %err,
            "failed to create BaiDec trajectory directory"
        );
        return;
    }
    let Ok(line) = serde_json::to_string(record) else {
        return;
    };
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(mut file) => {
            let _ = writeln!(file, "{line}");
        }
        Err(err) => {
            warn!(
                path = %path.display(),
                error = %err,
                "failed to append BaiDec trajectory record"
            );
        }
    }
}

#[cfg(test)]
#[path = "baidec_tests.rs"]
mod tests;
