use codex_api::CompactionInput;
use codex_api::OpenAiVerbosity;
use codex_api::ResponsesApiInputItem;
use codex_api::ResponsesApiRequest;
use codex_api::TextControls;
use codex_api::create_text_param_for_request;
use codex_protocol::config_types::ServiceTier;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::MessagePhase;
use pretty_assertions::assert_eq;

use super::*;

#[test]
fn serializes_text_verbosity_when_set() {
    let input: Vec<ResponsesApiInputItem> = vec![];
    let tools: Vec<serde_json::Value> = vec![];
    let req = ResponsesApiRequest {
        model: "gpt-5.4".to_string(),
        instructions: "i".to_string(),
        input,
        tools,
        tool_choice: "auto".to_string(),
        parallel_tool_calls: true,
        reasoning: None,
        store: false,
        stream: true,
        include: vec![],
        prompt_cache_key: None,
        service_tier: None,
        text: Some(TextControls {
            verbosity: Some(OpenAiVerbosity::Low),
            format: None,
        }),
        client_metadata: None,
    };

    let v = serde_json::to_value(&req).expect("json");
    assert_eq!(
        v.get("text")
            .and_then(|t| t.get("verbosity"))
            .and_then(|s| s.as_str()),
        Some("low")
    );
}

#[test]
fn serializes_text_schema_with_strict_format() {
    let input: Vec<ResponsesApiInputItem> = vec![];
    let tools: Vec<serde_json::Value> = vec![];
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "answer": {"type": "string"}
        },
        "required": ["answer"],
    });
    let text_controls = create_text_param_for_request(
        /*verbosity*/ None,
        &Some(schema.clone()),
        /*output_schema_strict*/ true,
    )
    .expect("text controls");

    let req = ResponsesApiRequest {
        model: "gpt-5.4".to_string(),
        instructions: "i".to_string(),
        input,
        tools,
        tool_choice: "auto".to_string(),
        parallel_tool_calls: true,
        reasoning: None,
        store: false,
        stream: true,
        include: vec![],
        prompt_cache_key: None,
        service_tier: None,
        text: Some(text_controls),
        client_metadata: None,
    };

    let v = serde_json::to_value(&req).expect("json");
    let text = v.get("text").expect("text field");
    assert!(text.get("verbosity").is_none());
    let format = text.get("format").expect("format field");

    assert_eq!(
        format.get("name"),
        Some(&serde_json::Value::String("codex_output_schema".into()))
    );
    assert_eq!(
        format.get("type"),
        Some(&serde_json::Value::String("json_schema".into()))
    );
    assert_eq!(format.get("strict"), Some(&serde_json::Value::Bool(true)));
    assert_eq!(format.get("schema"), Some(&schema));
}

#[test]
fn serializes_text_schema_with_non_strict_format() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "answer": {"type": "string"},
            "rationale": {"type": "string"}
        },
        "required": ["answer"],
        "additionalProperties": false
    });
    let text_controls = create_text_param_for_request(
        /*verbosity*/ None,
        &Some(schema.clone()),
        /*output_schema_strict*/ false,
    )
    .expect("text controls");

    let format = text_controls.format.expect("format field");
    assert!(!format.strict);
    assert_eq!(format.schema, schema);
}

#[test]
fn omits_text_when_not_set() {
    let input: Vec<ResponsesApiInputItem> = vec![];
    let tools: Vec<serde_json::Value> = vec![];
    let req = ResponsesApiRequest {
        model: "gpt-5.4".to_string(),
        instructions: "i".to_string(),
        input,
        tools,
        tool_choice: "auto".to_string(),
        parallel_tool_calls: true,
        reasoning: None,
        store: false,
        stream: true,
        include: vec![],
        prompt_cache_key: None,
        service_tier: None,
        text: None,
        client_metadata: None,
    };

    let v = serde_json::to_value(&req).expect("json");
    assert!(v.get("text").is_none());
}

#[test]
fn omits_internal_message_phase_from_responses_api_input() {
    let req = ResponsesApiRequest {
        model: "gpt-5.4".to_string(),
        instructions: "i".to_string(),
        input: vec![ResponsesApiInputItem::from(ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: "still working".to_string(),
            }],
            phase: Some(MessagePhase::Commentary),
        })],
        tools: vec![],
        tool_choice: "auto".to_string(),
        parallel_tool_calls: true,
        reasoning: None,
        store: false,
        stream: true,
        include: vec![],
        prompt_cache_key: None,
        service_tier: None,
        text: None,
        client_metadata: None,
    };

    let v = serde_json::to_value(&req).expect("json");
    let message = v
        .get("input")
        .and_then(|input| input.as_array())
        .and_then(|input| input.first())
        .expect("serialized input message");
    assert!(message.get("phase").is_none());
}

#[test]
fn serializes_assistant_message_status_for_responses_api_input() {
    let item = ResponsesApiInputItem::from(ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: "done".to_string(),
        }],
        phase: None,
    });

    let v = serde_json::to_value(&item).expect("json");
    assert_eq!(
        v.get("status").and_then(|status| status.as_str()),
        Some("completed")
    );
}

#[test]
fn omits_user_message_status_for_responses_api_input() {
    let item = ResponsesApiInputItem::from(ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "hello".to_string(),
        }],
        phase: None,
    });

    let v = serde_json::to_value(&item).expect("json");
    assert!(v.get("status").is_none());
}

#[test]
fn serializes_reasoning_status_for_responses_api_input() {
    let item = ResponsesApiInputItem::from(ResponseItem::Reasoning {
        id: "rs_1".to_string(),
        summary: Vec::new(),
        content: None,
        encrypted_content: Some("encrypted".to_string()),
    });

    let v = serde_json::to_value(&item).expect("json");
    assert_eq!(
        v.get("status").and_then(|status| status.as_str()),
        Some("completed")
    );
}

#[test]
fn serializes_function_call_status_for_responses_api_input() {
    let item = ResponsesApiInputItem::from(ResponseItem::FunctionCall {
        id: None,
        name: "exec_command".to_string(),
        namespace: None,
        arguments: "{}".to_string(),
        call_id: "call-1".to_string(),
    });

    let v = serde_json::to_value(&item).expect("json");
    assert_eq!(
        v.get("status").and_then(|status| status.as_str()),
        Some("completed")
    );
}

#[test]
fn serializes_function_call_output_status_for_responses_api_input() {
    let item = ResponsesApiInputItem::from(ResponseItem::FunctionCallOutput {
        call_id: "call-1".to_string(),
        output: FunctionCallOutputPayload::from_text("done".to_string()),
    });

    let v = serde_json::to_value(&item).expect("json");
    assert_eq!(
        v.get("status").and_then(|status| status.as_str()),
        Some("completed")
    );
}

#[test]
fn serializes_missing_call_statuses_for_responses_api_input() {
    let items = [
        ResponsesApiInputItem::from(ResponseItem::CustomToolCall {
            id: None,
            status: None,
            call_id: "call-custom".to_string(),
            name: "generate_image".to_string(),
            input: "{}".to_string(),
        }),
        ResponsesApiInputItem::from(ResponseItem::ToolSearchCall {
            id: None,
            call_id: Some("call-search".to_string()),
            status: None,
            execution: "exec-1".to_string(),
            arguments: serde_json::json!({}),
        }),
        ResponsesApiInputItem::from(ResponseItem::WebSearchCall {
            id: None,
            status: None,
            action: None,
        }),
    ];

    for item in items {
        let v = serde_json::to_value(&item).expect("json");
        assert_eq!(
            v.get("status").and_then(|status| status.as_str()),
            Some("completed"),
            "serialized item should include completed status: {v:?}"
        );
    }
}

#[test]
fn serializes_missing_call_statuses_for_compaction_input() {
    let input = vec![
        ResponseItem::FunctionCallOutput {
            call_id: "call-1".to_string(),
            output: FunctionCallOutputPayload::from_text("done".to_string()),
        },
        ResponseItem::CustomToolCall {
            id: None,
            status: None,
            call_id: "call-custom".to_string(),
            name: "generate_image".to_string(),
            input: "{}".to_string(),
        },
        ResponseItem::ToolSearchCall {
            id: None,
            call_id: Some("call-search".to_string()),
            status: None,
            execution: "exec-1".to_string(),
            arguments: serde_json::json!({}),
        },
        ResponseItem::WebSearchCall {
            id: None,
            status: None,
            action: None,
        },
    ];
    let payload = CompactionInput {
        model: "gpt-5.4",
        input: &input,
        instructions: "",
        tools: vec![],
        parallel_tool_calls: true,
        reasoning: None,
        service_tier: None,
        prompt_cache_key: None,
        text: None,
    };

    let v = serde_json::to_value(&payload).expect("json");
    let serialized_input = v
        .get("input")
        .and_then(|input| input.as_array())
        .expect("serialized compaction input");

    for item in serialized_input {
        assert_eq!(
            item.get("status").and_then(|status| status.as_str()),
            Some("completed"),
            "serialized compaction item should include completed status: {item:?}"
        );
    }
}

#[test]
fn serializes_flex_service_tier_when_set() {
    let req = ResponsesApiRequest {
        model: "gpt-5.4".to_string(),
        instructions: "i".to_string(),
        input: vec![],
        tools: vec![],
        tool_choice: "auto".to_string(),
        parallel_tool_calls: true,
        reasoning: None,
        store: false,
        stream: true,
        include: vec![],
        prompt_cache_key: None,
        service_tier: Some(ServiceTier::Flex.to_string()),
        text: None,
        client_metadata: None,
    };

    let v = serde_json::to_value(&req).expect("json");
    assert_eq!(
        v.get("service_tier").and_then(|tier| tier.as_str()),
        Some("flex")
    );
}

#[test]
fn reserializes_shell_outputs_for_function_and_custom_tool_calls() {
    let raw_output = r#"{"output":"hello","metadata":{"exit_code":0,"duration_seconds":0.5}}"#;
    let expected_output = "Exit code: 0\nWall time: 0.5 seconds\nOutput:\nhello";
    let mut items = vec![
        ResponseItem::FunctionCall {
            id: None,
            name: "shell".to_string(),
            namespace: None,
            arguments: "{}".to_string(),
            call_id: "call-1".to_string(),
        },
        ResponseItem::FunctionCallOutput {
            call_id: "call-1".to_string(),
            output: FunctionCallOutputPayload::from_text(raw_output.to_string()),
        },
        ResponseItem::CustomToolCall {
            id: None,
            status: None,
            call_id: "call-2".to_string(),
            name: "apply_patch".to_string(),
            input: "*** Begin Patch".to_string(),
        },
        ResponseItem::CustomToolCallOutput {
            call_id: "call-2".to_string(),
            name: None,
            output: FunctionCallOutputPayload::from_text(raw_output.to_string()),
        },
    ];

    reserialize_shell_outputs(&mut items);

    assert_eq!(
        items,
        vec![
            ResponseItem::FunctionCall {
                id: None,
                name: "shell".to_string(),
                namespace: None,
                arguments: "{}".to_string(),
                call_id: "call-1".to_string(),
            },
            ResponseItem::FunctionCallOutput {
                call_id: "call-1".to_string(),
                output: FunctionCallOutputPayload::from_text(expected_output.to_string()),
            },
            ResponseItem::CustomToolCall {
                id: None,
                status: None,
                call_id: "call-2".to_string(),
                name: "apply_patch".to_string(),
                input: "*** Begin Patch".to_string(),
            },
            ResponseItem::CustomToolCallOutput {
                call_id: "call-2".to_string(),
                name: None,
                output: FunctionCallOutputPayload::from_text(expected_output.to_string()),
            },
        ]
    );
}
