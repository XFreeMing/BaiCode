use std::process::Command;
use std::sync::Arc;

use codex_core::ModelClient;
use codex_core::Prompt;
use codex_core::ResponseEvent;
use codex_features::Feature;
use codex_login::CodexAuth;
use codex_login::auth::AgentIdentityAuthPolicy;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::WireApi;
use codex_otel::SessionTelemetry;
use codex_otel::TelemetryAuthMode;
use codex_protocol::ThreadId;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_thread_context_ids_test_support::thread_context_ids;
use core_test_support::TestCodexResponsesRequestKind;
use core_test_support::load_default_config_for_test;
use core_test_support::responses;
use core_test_support::responses_metadata as test_responses_metadata;
use core_test_support::test_codex::test_codex;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

const TEST_INSTALLATION_ID: &str = "11111111-1111-4111-8111-111111111111";
fn test_turn_responses_metadata(
    _client: &ModelClient,
    thread_id: ThreadId,
    session_source: &SessionSource,
) -> codex_core::CodexResponsesMetadata {
    let thread_id = thread_id.to_string();
    test_responses_metadata(
        TEST_INSTALLATION_ID,
        &thread_id,
        &thread_id,
        /*turn_id*/ None,
        format!("{thread_id}:0"),
        session_source,
        /*parent_thread_id*/ None,
        TestCodexResponsesRequestKind::Turn,
    )
}

#[tokio::test]
async fn responses_stream_omits_subagent_header_on_review() {
    core_test_support::skip_if_no_network!();

    let server = responses::start_mock_server().await;
    let response_body = responses::sse(vec![
        responses::ev_response_created("resp-1"),
        responses::ev_completed("resp-1"),
    ]);

    let request_recorder = responses::mount_sse_once(&server, response_body).await;

    let provider = ModelProviderInfo {
        name: "mock".into(),
        base_url: Some(format!("{}/v1", server.uri())),
        model_catalog_url: None,
        env_key: None,
        env_key_instructions: None,
        experimental_bearer_token: None,
        auth: None,
        gateway_oauth: None,
        aws: None,
        wire_api: WireApi::Responses,
        query_params: None,
        http_headers: None,
        env_http_headers: None,
        request_max_retries: Some(0),
        stream_max_retries: Some(0),
        stream_idle_timeout_ms: Some(5_000),
        websocket_connect_timeout_ms: None,
        requires_openai_auth: false,
        supports_websockets: false,
        supports_standalone_web_search: false,
        capabilities: None,
        project_id: None,
        location: None,
        credential_file: None,
        include_internal_metadata: false,
    };

    let codex_home = TempDir::new().expect("failed to create TempDir");
    let mut config = load_default_config_for_test(&codex_home).await;
    config.model_provider_id = provider.name.clone();
    config.model_provider = provider.clone();
    let effort = config.model_reasoning_effort.clone();
    let summary = config.model_reasoning_summary;
    let model = codex_core::test_support::get_model_offline(config.model.as_deref());
    config.model = Some(model.clone());
    let config = Arc::new(config);

    let thread_id = ThreadId::new();
    let auth_mode = TelemetryAuthMode::Chatgpt;
    let session_source = SessionSource::SubAgent(SubAgentSource::Review);
    let model_info =
        codex_core::test_support::construct_model_info_offline(model.as_str(), &config);
    let session_telemetry = SessionTelemetry::new(
        thread_id,
        model.as_str(),
        model_info.slug.as_str(),
        /*account_id*/ None,
        Some("test@test.com".to_string()),
        Some(auth_mode),
        "test_originator".to_string(),
        /*log_user_prompts*/ false,
        "test".to_string(),
        session_source.clone(),
    );

    let client = ModelClient::new(
        codex_model_provider::create_model_provider(provider.clone(), /*auth_manager*/ None),
        AgentIdentityAuthPolicy::JwtOnly,
        thread_id,
        thread_context_ids(),
        session_source.clone(),
        "test_originator".to_string(),
        config.model_verbosity,
        config.features.enabled(Feature::ContentItemKinds),
        config.features.enabled(Feature::ReasoningEffortOverride),
        /*enable_request_compression*/ false,
        /*include_timing_metrics*/ false,
        /*beta_features_header*/ None,
        /*concurrent_reasoning_summaries_enabled*/ false,
        /*output_token_replay_enabled*/ false,
        /*attestation_provider*/ None,
        config.http_client_factory(),
        config.workspace_routing_context(),
        Vec::new(),
    );
    let responses_metadata = test_turn_responses_metadata(&client, thread_id, &session_source);
    let mut client_session = client.new_session();

    let mut prompt = Prompt::default();
    prompt.input = vec![ResponseItem::Message {
        status: None,
        encrypted_content: None,
        id: None,
        role: "user".into(),
        content: vec![ContentItem::InputText {
            text: "hello".into(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }];

    let mut stream = client_session
        .stream(
            &Arc::new(prompt),
            &model_info,
            &session_telemetry,
            effort,
            summary.unwrap_or(model_info.default_reasoning_summary),
            /*service_tier*/ None,
            &responses_metadata,
            &codex_rollout_trace::InferenceTraceContext::disabled(),
        )
        .await
        .expect("stream failed");
    while let Some(event) = stream.next().await {
        if matches!(event, Ok(ResponseEvent::Completed { .. })) {
            break;
        }
    }

    let request = request_recorder.single_request();
    assert!(request.body_json().get("client_metadata").is_none());
    for key in [
        "x-openai-subagent",
        "x-codex-window-id",
        "x-codex-parent-thread-id",
        "x-codex-installation-id",
    ] {
        assert!(request.header(key).is_none(), "{key}");
    }
}

#[tokio::test]
async fn responses_stream_omits_subagent_header_on_other() {
    core_test_support::skip_if_no_network!();

    let server = responses::start_mock_server().await;
    let response_body = responses::sse(vec![
        responses::ev_response_created("resp-1"),
        responses::ev_completed("resp-1"),
    ]);

    let request_recorder = responses::mount_sse_once(&server, response_body).await;

    let provider = ModelProviderInfo {
        name: "mock".into(),
        base_url: Some(format!("{}/v1", server.uri())),
        model_catalog_url: None,
        env_key: None,
        env_key_instructions: None,
        experimental_bearer_token: None,
        auth: None,
        gateway_oauth: None,
        aws: None,
        wire_api: WireApi::Responses,
        query_params: None,
        http_headers: None,
        env_http_headers: None,
        request_max_retries: Some(0),
        stream_max_retries: Some(0),
        stream_idle_timeout_ms: Some(5_000),
        websocket_connect_timeout_ms: None,
        requires_openai_auth: false,
        supports_websockets: false,
        supports_standalone_web_search: false,
        capabilities: None,
        project_id: None,
        location: None,
        credential_file: None,
        include_internal_metadata: false,
    };

    let codex_home = TempDir::new().expect("failed to create TempDir");
    let mut config = load_default_config_for_test(&codex_home).await;
    config.model_provider_id = provider.name.clone();
    config.model_provider = provider.clone();
    let effort = config.model_reasoning_effort.clone();
    let summary = config.model_reasoning_summary;
    let model = codex_core::test_support::get_model_offline(config.model.as_deref());
    config.model = Some(model.clone());
    let config = Arc::new(config);

    let thread_id = ThreadId::new();
    let auth_mode = TelemetryAuthMode::Chatgpt;
    let session_source = SessionSource::SubAgent(SubAgentSource::Other("my-task".to_string()));
    let model_info =
        codex_core::test_support::construct_model_info_offline(model.as_str(), &config);

    let session_telemetry = SessionTelemetry::new(
        thread_id,
        model.as_str(),
        model_info.slug.as_str(),
        /*account_id*/ None,
        Some("test@test.com".to_string()),
        Some(auth_mode),
        "test_originator".to_string(),
        /*log_user_prompts*/ false,
        "test".to_string(),
        session_source.clone(),
    );

    let client = ModelClient::new(
        codex_model_provider::create_model_provider(provider.clone(), /*auth_manager*/ None),
        AgentIdentityAuthPolicy::JwtOnly,
        thread_id,
        thread_context_ids(),
        session_source.clone(),
        "test_originator".to_string(),
        config.model_verbosity,
        config.features.enabled(Feature::ContentItemKinds),
        config.features.enabled(Feature::ReasoningEffortOverride),
        /*enable_request_compression*/ false,
        /*include_timing_metrics*/ false,
        /*beta_features_header*/ None,
        /*concurrent_reasoning_summaries_enabled*/ false,
        /*output_token_replay_enabled*/ false,
        /*attestation_provider*/ None,
        config.http_client_factory(),
        config.workspace_routing_context(),
        Vec::new(),
    );
    let responses_metadata = test_turn_responses_metadata(&client, thread_id, &session_source);
    let mut client_session = client.new_session();

    let mut prompt = Prompt::default();
    prompt.input = vec![ResponseItem::Message {
        status: None,
        encrypted_content: None,
        id: None,
        role: "user".into(),
        content: vec![ContentItem::InputText {
            text: "hello".into(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }];

    let mut stream = client_session
        .stream(
            &Arc::new(prompt),
            &model_info,
            &session_telemetry,
            effort,
            summary.unwrap_or(model_info.default_reasoning_summary),
            /*service_tier*/ None,
            &responses_metadata,
            &codex_rollout_trace::InferenceTraceContext::disabled(),
        )
        .await
        .expect("stream failed");
    while let Some(event) = stream.next().await {
        if matches!(event, Ok(ResponseEvent::Completed { .. })) {
            break;
        }
    }

    let request = request_recorder.single_request();
    assert_eq!(request.header("x-openai-subagent").as_deref(), None);
}

#[tokio::test]
async fn responses_respects_model_info_overrides_from_config() {
    core_test_support::skip_if_no_network!();

    let server = responses::start_mock_server().await;
    let response_body = responses::sse(vec![
        responses::ev_response_created("resp-1"),
        responses::ev_completed("resp-1"),
    ]);

    let request_recorder = responses::mount_sse_once(&server, response_body).await;

    let provider = ModelProviderInfo {
        name: "mock".into(),
        base_url: Some(format!("{}/v1", server.uri())),
        model_catalog_url: None,
        env_key: None,
        env_key_instructions: None,
        experimental_bearer_token: None,
        auth: None,
        gateway_oauth: None,
        aws: None,
        wire_api: WireApi::Responses,
        query_params: None,
        http_headers: None,
        env_http_headers: None,
        request_max_retries: Some(0),
        stream_max_retries: Some(0),
        stream_idle_timeout_ms: Some(5_000),
        websocket_connect_timeout_ms: None,
        requires_openai_auth: false,
        supports_websockets: false,
        supports_standalone_web_search: false,
        capabilities: None,
        project_id: None,
        location: None,
        credential_file: None,
        include_internal_metadata: false,
    };

    let codex_home = TempDir::new().expect("failed to create TempDir");
    let mut config = load_default_config_for_test(&codex_home).await;
    config.model = Some("gpt-3.5-turbo".to_string());
    config.model_provider_id = provider.name.clone();
    config.model_provider = provider.clone();
    config.model_reasoning_summary = Some(ReasoningSummary::Detailed);
    let effort = config.model_reasoning_effort.clone();
    let summary = config.model_reasoning_summary;
    let model = config.model.clone().expect("model configured");
    let config = Arc::new(config);

    let thread_id = ThreadId::new();
    let auth_mode =
        codex_core::test_support::auth_manager_from_auth(CodexAuth::from_api_key("Test API Key"))
            .auth_mode()
            .map(TelemetryAuthMode::from);
    let session_source =
        SessionSource::SubAgent(SubAgentSource::Other("override-check".to_string()));
    let model_info =
        codex_core::test_support::construct_model_info_offline(model.as_str(), &config);
    let session_telemetry = SessionTelemetry::new(
        thread_id,
        model.as_str(),
        model_info.slug.as_str(),
        /*account_id*/ None,
        Some("test@test.com".to_string()),
        auth_mode,
        "test_originator".to_string(),
        /*log_user_prompts*/ false,
        "test".to_string(),
        session_source.clone(),
    );

    let client = ModelClient::new(
        codex_model_provider::create_model_provider(provider.clone(), /*auth_manager*/ None),
        AgentIdentityAuthPolicy::JwtOnly,
        thread_id,
        thread_context_ids(),
        session_source.clone(),
        "test_originator".to_string(),
        config.model_verbosity,
        config.features.enabled(Feature::ContentItemKinds),
        config.features.enabled(Feature::ReasoningEffortOverride),
        /*enable_request_compression*/ false,
        /*include_timing_metrics*/ false,
        /*beta_features_header*/ None,
        /*concurrent_reasoning_summaries_enabled*/ false,
        /*output_token_replay_enabled*/ false,
        /*attestation_provider*/ None,
        config.http_client_factory(),
        config.workspace_routing_context(),
        Vec::new(),
    );
    let responses_metadata = test_turn_responses_metadata(&client, thread_id, &session_source);
    let mut client_session = client.new_session();

    let mut prompt = Prompt::default();
    prompt.input = vec![ResponseItem::Message {
        status: None,
        encrypted_content: None,
        id: None,
        role: "user".into(),
        content: vec![ContentItem::InputText {
            text: "hello".into(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }];

    let mut stream = client_session
        .stream(
            &Arc::new(prompt),
            &model_info,
            &session_telemetry,
            effort,
            summary.unwrap_or(model_info.default_reasoning_summary),
            /*service_tier*/ None,
            &responses_metadata,
            &codex_rollout_trace::InferenceTraceContext::disabled(),
        )
        .await
        .expect("stream failed");
    while let Some(event) = stream.next().await {
        if matches!(event, Ok(ResponseEvent::Completed { .. })) {
            break;
        }
    }

    let request = request_recorder.single_request();
    let body = request.body_json();
    let reasoning = body
        .get("reasoning")
        .and_then(|value| value.as_object())
        .cloned();

    assert!(
        reasoning.is_some(),
        "reasoning should be present when config enables summaries"
    );

    assert_eq!(
        reasoning
            .as_ref()
            .and_then(|value| value.get("summary"))
            .and_then(|value| value.as_str()),
        Some("detailed")
    );
}

#[tokio::test]
async fn responses_omit_workspace_metadata_across_tool_calls() {
    core_test_support::skip_if_no_network!();

    let server = responses::start_mock_server().await;
    let test = test_codex()
        .with_model("test-gpt-5.1-codex")
        .build(&server)
        .await
        .expect("build test codex");
    let cwd = test.cwd_path();
    let credential_bearing_origin =
        "https://alice:test-git-secret-token@example.invalid/openai/codex.git";

    for args in [
        ["init"].as_slice(),
        ["remote", "add", "origin", credential_bearing_origin].as_slice(),
    ] {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git command should run");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let first_response = responses::sse(vec![
        responses::ev_response_created("resp-1"),
        responses::ev_function_call(
            "wait-for-git",
            "test_sync_tool",
            r#"{"wait_for_git_enrichment":true}"#,
        ),
        responses::ev_completed("resp-1"),
    ]);
    let follow_up_response = responses::sse(vec![
        responses::ev_response_created("resp-2"),
        responses::ev_assistant_message("msg-1", "done"),
        responses::ev_completed("resp-2"),
    ]);
    let request_log = responses::mount_response_sequence(
        &server,
        vec![
            responses::sse_response(first_response),
            responses::sse_response(follow_up_response),
        ],
    )
    .await;

    test.submit_turn("inspect the workspace")
        .await
        .expect("submit turn prompt");

    let requests = request_log.requests();
    assert_eq!(requests.len(), 2, "expected a follow-up model request");
    for request in requests {
        let body = request.body_json();
        assert!(body.get("client_metadata").is_none());
        assert!(body.get("prompt_cache_key").is_none());
        assert!(!body.to_string().contains("test-git-secret-token"));
        for key in [
            "x-codex-turn-metadata",
            "x-codex-installation-id",
            "session-id",
            "thread-id",
            "traceparent",
        ] {
            assert!(request.header(key).is_none(), "{key}");
        }
    }
}
