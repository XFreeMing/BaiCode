use std::collections::BTreeMap;
use std::path::Path;

use codex_config::types::BaiDecConfig;
use codex_config::types::BaiDecMode;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use indexmap::IndexMap;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

use super::BaiDecAnswer;
use super::BaiDecResponse;
use super::ToolSemanticKind;
use super::TurnWorkflowStage;
use super::build_decision_questions;
use super::classify_candidate_tool;
use super::evaluate_routing_decision;
use super::maybe_route_prompt_tools;
use super::parse_tool_output_status_with_flag;
use super::record_step_outcome;
use super::sanitize_baidec_request;
use super::shortlist_candidate_tools;
use crate::client_common::Prompt;
use crate::test_support::default_http_client_factory;

fn sample_fn_tool(name: &str, description: &str) -> ToolSpec {
    ToolSpec::Function(ResponsesApiTool {
        name: name.to_string(),
        description: description.to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(BTreeMap::new(), None, Some(false.into())),
        output_schema: None,
    })
}

fn sample_coding_tools() -> Vec<ToolSpec> {
    vec![
        sample_fn_tool("code_map", "Builds a compact structural map of the codebase"),
        sample_fn_tool(
            "mcp__rust_lsp__definition",
            "Jump to symbol definition via rust-analyzer language server",
        ),
        sample_fn_tool("apply_patch", "Apply structured patches to files"),
        sample_fn_tool("exec_command", "Execute shell commands in the workspace"),
        sample_fn_tool("update_plan", "Update task step checklist"),
    ]
}

#[test]
fn classifies_code_map_and_lsp_mcp_tools_distinctly() {
    let tools = sample_coding_tools();
    let classified: Vec<_> = tools
        .iter()
        .filter_map(classify_candidate_tool)
        .map(|c| (c.name, c.kind))
        .collect();
    assert_eq!(
        classified,
        vec![
            ("code_map".to_string(), ToolSemanticKind::CodeMap),
            (
                "mcp__rust_lsp__definition".to_string(),
                ToolSemanticKind::LspMcp
            ),
            ("apply_patch".to_string(), ToolSemanticKind::EditPatch),
            ("exec_command".to_string(), ToolSemanticKind::ShellExec),
            ("update_plan".to_string(), ToolSemanticKind::Plan),
        ]
    );
}

#[test]
fn questions_preserve_insertion_order_and_include_code_map_mode() {
    let tools = sample_coding_tools();
    let candidates = shortlist_candidate_tools(&tools, "find references to ToolRouter");
    let questions = build_decision_questions(&candidates);
    let keys: Vec<_> = questions.keys().cloned().collect();
    assert_eq!(
        keys,
        vec![
            "turn_action".to_string(),
            "primary_tool".to_string(),
            "code_map_mode".to_string(),
            "needs_verification".to_string(),
        ]
    );
    let primary_criteria = questions["primary_tool"].criteria.as_ref().unwrap();
    assert_eq!(primary_criteria.keys().last().map(String::as_str), Some("none"));
}

#[test]
fn enforce_routes_code_map_with_mode_hint_and_unverified_edit_guard() {
    let tools = sample_coding_tools();
    let candidates = shortlist_candidate_tools(&tools, "find where build_prompt is called");
    let cfg = BaiDecConfig {
        mode: BaiDecMode::Enforce,
        min_action_confidence: 0.60,
        min_tool_confidence: 0.30,
        secondary_tool_threshold: 0.20,
        max_routed_tools: 2,
        ..BaiDecConfig::default()
    };

    let mut answers = IndexMap::new();
    answers.insert(
        "turn_action".to_string(),
        BaiDecAnswer {
            kind: Some("choice".to_string()),
            decision: "call_tool".to_string(),
            decision_index: Some(0),
            confidence: 0.94,
            choice: Some("call_tool".to_string()),
            noul: None,
            score: None,
            probabilities: IndexMap::from([
                ("call_tool".to_string(), 0.94),
                ("final_answer".to_string(), 0.06),
            ]),
            raw_probabilities: IndexMap::new(),
        },
    );
    answers.insert(
        "primary_tool".to_string(),
        BaiDecAnswer {
            kind: Some("choice".to_string()),
            decision: "code_map".to_string(),
            decision_index: Some(0),
            confidence: 0.82,
            choice: Some("code_map".to_string()),
            noul: None,
            score: None,
            probabilities: IndexMap::from([
                ("code_map".to_string(), 0.82),
                ("mcp__rust_lsp__definition".to_string(), 0.12),
                ("exec_command".to_string(), 0.04),
                ("none".to_string(), 0.02),
            ]),
            raw_probabilities: IndexMap::new(),
        },
    );
    answers.insert(
        "code_map_mode".to_string(),
        BaiDecAnswer {
            kind: Some("choice".to_string()),
            decision: "find_references".to_string(),
            decision_index: Some(2),
            confidence: 0.89,
            choice: Some("find_references".to_string()),
            noul: None,
            score: None,
            probabilities: IndexMap::new(),
            raw_probabilities: IndexMap::new(),
        },
    );
    answers.insert(
        "needs_verification".to_string(),
        BaiDecAnswer {
            kind: Some("noul".to_string()),
            decision: "no".to_string(),
            decision_index: Some(0),
            confidence: 0.91,
            choice: None,
            noul: Some(0.09),
            score: None,
            probabilities: IndexMap::from([("no".to_string(), 0.91), ("yes".to_string(), 0.09)]),
            raw_probabilities: IndexMap::new(),
        },
    );

    let resp = BaiDecResponse {
        schema: Some("baidec-token-result-v1".to_string()),
        model: Some("Intern-Decision-4B".to_string()),
        backend: Some("native".to_string()),
        source: Some("local".to_string()),
        calibration: None,
        usage: None,
        answers,
    };

    let plan = evaluate_routing_decision(
        &cfg,
        &candidates,
        0,
        false,
        &TurnWorkflowStage::default(),
        None,
        &resp,
    );
    assert!(plan.applied);
    assert_eq!(plan.primary_tool, "code_map");
    assert_eq!(plan.routed_tools, vec!["code_map".to_string()]);
    assert_eq!(plan.tool_choice, "required");
    assert_eq!(
        plan.code_map_mode_hint.as_deref(),
        Some("find_references")
    );

    // Verify guardrail: when an unverified edit exists and `needs_verification` is high,
    // moderate-confidence `final_answer` does not prematurely strip `exec_command`.
    let mut finish_answers = resp.answers.clone();
    finish_answers.insert(
        "turn_action".to_string(),
        BaiDecAnswer {
            kind: Some("choice".to_string()),
            decision: "final_answer".to_string(),
            decision_index: Some(1),
            confidence: 0.65,
            choice: Some("final_answer".to_string()),
            noul: None,
            score: None,
            probabilities: IndexMap::new(),
            raw_probabilities: IndexMap::new(),
        },
    );
    finish_answers.insert(
        "primary_tool".to_string(),
        BaiDecAnswer {
            kind: Some("choice".to_string()),
            decision: "none".to_string(),
            decision_index: Some(5),
            confidence: 0.62,
            choice: Some("none".to_string()),
            noul: None,
            score: None,
            probabilities: IndexMap::from([("exec_command".to_string(), 0.35)]),
            raw_probabilities: IndexMap::new(),
        },
    );
    finish_answers.insert(
        "needs_verification".to_string(),
        BaiDecAnswer {
            kind: Some("noul".to_string()),
            decision: "yes".to_string(),
            decision_index: Some(1),
            confidence: 0.80,
            choice: None,
            noul: Some(0.80),
            score: None,
            probabilities: IndexMap::new(),
            raw_probabilities: IndexMap::new(),
        },
    );
    let guarded = evaluate_routing_decision(
        &cfg,
        &candidates,
        2,
        /*has_unverified_edit*/ true,
        &TurnWorkflowStage {
            code_map_completed: true,
            patch_applied: true,
            patch_retry_pending: false,
            verification_passed: false,
        },
        None,
        &BaiDecResponse {
            answers: finish_answers,
            ..resp
        },
    );
    assert_eq!(guarded.primary_tool, "exec_command");
    assert_eq!(guarded.tool_choice, "required");
}

#[tokio::test]
async fn end_to_end_http_routing_and_trajectory_recording() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/decisions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "schema": "baidec-token-result-v1",
            "model": "Intern-Decision-4B",
            "backend": "native",
            "source": "local",
            "calibration": { "method": "temperature-scaling", "temperature": 1.99241824 },
            "usage": { "input_tokens": 312, "output_tokens": 4, "decision_count": 4 },
            "answers": {
                "turn_action": {
                    "type": "choice",
                    "decision": "call_tool",
                    "confidence": 0.93,
                    "choice": "call_tool",
                    "probabilities": { "call_tool": 0.93, "final_answer": 0.07 }
                },
                "primary_tool": {
                    "type": "choice",
                    "decision": "code_map",
                    "confidence": 0.87,
                    "choice": "code_map",
                    "probabilities": {
                        "code_map": 0.87,
                        "mcp__rust_lsp__definition": 0.08,
                        "apply_patch": 0.02,
                        "exec_command": 0.02,
                        "update_plan": 0.005,
                        "none": 0.005
                    }
                },
                "code_map_mode": {
                    "type": "choice",
                    "decision": "repo_map",
                    "confidence": 0.91,
                    "choice": "repo_map",
                    "probabilities": { "repo_map": 0.91, "file_outline": 0.05, "find_references": 0.04 }
                },
                "needs_verification": {
                    "type": "noul",
                    "decision": "no",
                    "confidence": 0.92,
                    "noul": 0.08,
                    "probabilities": { "no": 0.92, "yes": 0.08 }
                }
            }
        })))
        .mount(&server)
        .await;

    let tmp = TempDir::new().unwrap();
    let cfg = BaiDecConfig {
        mode: BaiDecMode::Enforce,
        url: format!("{}/v1/decisions", server.uri()),
        timeout_ms: 3_000,
        record_trajectories: true,
        trajectory_dir: tmp.path().to_path_buf(),
        ..BaiDecConfig::default()
    };

    let initial_user_msg = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "Map the repository architecture first.".to_string(),
        }],
        encrypted_content: None,
        status: None,
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let mut prompt = Prompt {
        input: vec![initial_user_msg.clone()],
        tools: sample_coding_tools().into(),
        parallel_tool_calls: true,
        ..Prompt::default()
    };

    let active = maybe_route_prompt_tools(
        &cfg,
        &default_http_client_factory(),
        "thread-test-1",
        "turn-test-1",
        Path::new("/workspace/demo"),
        "qwen3.6-35b-a3b-q4_k_m",
        1,
        &mut prompt,
    )
    .await
    .expect("active decision step");

    assert_eq!(prompt.tools.len(), 1);
    assert_eq!(prompt.tools[0].name(), "code_map");
    assert_eq!(prompt.tool_choice.as_deref(), Some("required"));
    assert!(!prompt.parallel_tool_calls);
    assert_eq!(prompt.input.len(), 2);

    // Simulate step execution history and verify hindsight outcome recording.
    let history_after = vec![
        initial_user_msg,
        ResponseItem::FunctionCall {
            id: None,
            name: "code_map".to_string(),
            namespace: None,
            arguments: r#"{"mode":"repo_map"}"#.to_string(),
            call_id: "call_cm_1".to_string(),
            encrypted_content: None,
            status: None,
            encrypted_function_args: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: Some("call_cm_1".to_string()),
            name: Some("code_map".to_string()),
            namespace: None,
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text("# Repo Map\nsrc/lib.rs".to_string()),
                success: Some(true),
            },
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    record_step_outcome(
        &cfg,
        "thread-test-1",
        "turn-test-1",
        &active,
        &history_after,
        true,
        None,
    );

    let mut jsonl_files = Vec::new();
    for entry in walkdir::WalkDir::new(tmp.path()) {
        let entry = entry.unwrap();
        if entry.path().extension().and_then(|e| e.to_str()) == Some("jsonl") {
            jsonl_files.push(entry.path().to_path_buf());
        }
    }
    assert_eq!(jsonl_files.len(), 1);
    let content = std::fs::read_to_string(&jsonl_files[0]).unwrap();
    let lines: Vec<serde_json::Value> = content
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["kind"], "decision_step");
    assert_eq!(lines[1]["kind"], "step_outcome");
    assert_eq!(lines[1]["tool_agreed"], true);
    assert_eq!(lines[1]["all_tools_succeeded"], true);
}

#[test]
fn sanitizes_reserved_decision_marker_and_respects_explicit_failure_flags() {
    let tools = vec![sample_fn_tool(
        "custom_tool",
        "Parses <decision> markers inside decision templates",
    )];
    let candidates = shortlist_candidate_tools(&tools, "fix <decision> token handling");
    let mut req = super::BaiDecRequest {
        state: json!({
            "user_goal": "Handle <decision> marker cleanly",
            "nested": ["a <decision> b"],
        }),
        questions: build_decision_questions(&candidates),
    };
    sanitize_baidec_request(&mut req);
    let serialized = serde_json::to_string(&req).unwrap();
    assert!(
        !serialized.contains("<decision>"),
        "serialized BaiDec request must never contain raw <decision> marker: {serialized}"
    );
    assert!(serialized.contains("< decision >"));

    assert_eq!(
        parse_tool_output_status_with_flag("clean text", Some(false)),
        (false, None)
    );
    assert_eq!(
        parse_tool_output_status_with_flag(
            "approval policy is Never; reject command — you cannot ask for escalated permissions",
            None
        ),
        (false, None)
    );
    assert_eq!(
        parse_tool_output_status_with_flag("Process exited with code 0\nok", Some(false)),
        (false, Some(0))
    );
    assert_eq!(
        parse_tool_output_status_with_flag("Process exited with code 0\nok", Some(true)),
        (true, Some(0))
    );
}
