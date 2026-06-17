use std::time::Duration;

use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseInputItem;
use serde::Deserialize;
use serde_json::Value as JsonValue;
use serde_json::json;

use crate::function_tool::FunctionCallError;
use crate::stream_events_utils::image_generation_artifact_call_id;
use crate::stream_events_utils::save_image_generation_result;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;
use codex_login::default_client::create_client;
use codex_tools::GENERATE_IMAGE_TOOL_NAME;
use codex_tools::ToolName;
use codex_utils_absolute_path::AbsolutePathBuf;

/// Per-turn id header (§6.1): codex carries `turn_context.sub_id` so the ADG
/// `/v1/images/*` endpoint can enforce the deterministic per-turn breaker.
const ADG_TURN_ID_HEADER: &str = "X-ADG-Turn-Id";
/// Request-scoped image model channel (§2): for multipart `/edits` ADG cannot
/// read the JSON body, so the chosen model rides this header on every call.
const ADG_IMAGE_MODEL_HEADER: &str = "X-ADG-Image-Model";
/// Image generation is a single request/response with no agent loop, but the
/// upstream render itself can take tens of seconds (≈37s measured), so the
/// timeout is generous relative to ordinary API calls.
const IMAGE_GENERATION_TIMEOUT: Duration = Duration::from_secs(120);

pub struct GenerateImageHandler;

#[derive(Deserialize)]
struct GenerateImageArgs {
    prompt: String,
    size: Option<String>,
    model: Option<String>,
    reference_image_paths: Option<Vec<String>>,
}

/// Structured outcome of a `generate_image` call, surfaced to the model as the
/// function_call_output. The discriminated status is what lets the model decide
/// its next step on its own: `generated` -> reference the saved path and stop;
/// `refused` -> terminal, report to the user, do not retry; `failed` -> act per
/// `retryable`. See ai-development-gateway
/// docs/generate-image-function-tool-2026-06-17.zh.md §3b.
pub enum GenerateImageOutcome {
    Generated {
        saved_path: String,
        size: Option<String>,
        model: Option<String>,
        /// Data URL of the generated image so the model can perceive its own
        /// output (drives "good enough -> stop" vs "iterate" decisions).
        image_url: String,
    },
    Refused {
        code: String,
        message: String,
    },
    Failed {
        code: String,
        message: String,
        retryable: bool,
    },
}

impl GenerateImageOutcome {
    fn status_json(&self) -> JsonValue {
        match self {
            GenerateImageOutcome::Generated {
                saved_path,
                size,
                model,
                ..
            } => json!({
                "status": "generated",
                "saved_path": saved_path,
                "size": size,
                "model": model,
            }),
            GenerateImageOutcome::Refused { code, message } => json!({
                "status": "refused",
                "refusal": { "code": code, "message": message },
            }),
            GenerateImageOutcome::Failed {
                code,
                message,
                retryable,
            } => json!({
                "status": "failed",
                "error": { "code": code, "message": message, "retryable": retryable },
            }),
        }
    }

    fn is_generated(&self) -> bool {
        matches!(self, GenerateImageOutcome::Generated { .. })
    }
}

pub struct GenerateImageOutput {
    outcome: GenerateImageOutcome,
}

impl ToolOutput for GenerateImageOutput {
    fn log_preview(&self) -> String {
        self.outcome.status_json().to_string()
    }

    fn success_for_logging(&self) -> bool {
        self.outcome.is_generated()
    }

    fn to_response_item(&self, call_id: &str, _payload: &ToolPayload) -> ResponseInputItem {
        // Always surface the structured status as text so the model has a
        // legible result in every case (success, refusal, failure). On success
        // additionally attach the image as a viewable input so the model can
        // perceive what it produced.
        let mut items = vec![FunctionCallOutputContentItem::InputText {
            text: self.outcome.status_json().to_string(),
        }];
        if let GenerateImageOutcome::Generated { image_url, .. } = &self.outcome {
            items.push(FunctionCallOutputContentItem::InputImage {
                image_url: image_url.clone(),
                detail: None,
            });
        }
        ResponseInputItem::FunctionCallOutput {
            call_id: call_id.to_string(),
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::ContentItems(items),
                success: Some(self.outcome.is_generated()),
            },
        }
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        self.outcome.status_json()
    }
}

impl ToolHandler for GenerateImageHandler {
    type Output = GenerateImageOutput;

    fn tool_name(&self) -> ToolName {
        ToolName::plain(GENERATE_IMAGE_TOOL_NAME)
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    async fn handle(&self, invocation: ToolInvocation) -> Result<Self::Output, FunctionCallError> {
        let arguments = match &invocation.payload {
            ToolPayload::Function { arguments } => arguments.clone(),
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "generate_image handler received unsupported payload".to_string(),
                ));
            }
        };
        let args: GenerateImageArgs = parse_arguments(&arguments)?;

        // Reference-image editing routes to `/v1/images/edits` (multipart),
        // which this build does not wire yet. Surface an explicit, terminal
        // failure rather than silently dropping the reference images.
        // `image_reference_unsupported` is a client-only code (the gateway never
        // emits it), tracked as such in the §3b contract.
        if args
            .reference_image_paths
            .as_ref()
            .is_some_and(|paths| !paths.is_empty())
        {
            return Ok(GenerateImageOutput {
                outcome: GenerateImageOutcome::Failed {
                    code: "image_reference_unsupported".to_string(),
                    message:
                        "reference-image editing is not available yet; describe the desired change in the prompt instead"
                            .to_string(),
                    retryable: false,
                },
            });
        }

        let outcome = generate_via_images_api(&invocation, &args).await?;
        Ok(GenerateImageOutput { outcome })
    }
}

/// POST the prompt to the provider's `/v1/images/generations` and translate the
/// HTTP outcome into a §3b structured result. Returns `Err` only for genuine
/// pre-flight defects (an unconfigured provider, an unwritable codex_home) —
/// every endpoint-reported condition becomes a `GenerateImageOutcome` so the
/// model can decide its next step. The endpoint owns capability gating, account
/// selection, billing, and the deterministic breaker; this is a translation
/// layer that swallows nothing.
async fn generate_via_images_api(
    invocation: &ToolInvocation,
    args: &GenerateImageArgs,
) -> Result<GenerateImageOutcome, FunctionCallError> {
    let turn = invocation.turn.as_ref();
    let base_url = turn
        .provider
        .runtime_base_url()
        .await
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "generate_image: cannot resolve provider base_url: {err}"
            ))
        })?
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "generate_image: provider base_url is not configured".to_string(),
            )
        })?;
    let auth = turn.provider.api_auth().await.map_err(|err| {
        FunctionCallError::RespondToModel(format!(
            "generate_image: cannot resolve provider auth: {err}"
        ))
    })?;

    fulfil_via_generations(
        &base_url,
        auth.to_auth_headers(),
        &turn.sub_id,
        &turn.config.codex_home,
        &invocation.session.conversation_id.to_string(),
        &invocation.call_id,
        args,
    )
    .await
}

/// The networking core, decoupled from `TurnContext` so it can be exercised
/// end-to-end against a local mock server. Sends the authenticated POST, then
/// maps status/body to a §3b outcome; `Err` only on an unwritable codex_home.
async fn fulfil_via_generations(
    base_url: &str,
    auth_headers: http::HeaderMap,
    turn_id: &str,
    codex_home: &AbsolutePathBuf,
    session_id: &str,
    call_id: &str,
    args: &GenerateImageArgs,
) -> Result<GenerateImageOutcome, FunctionCallError> {
    let url = format!("{}/images/generations", base_url.trim_end_matches('/'));
    let body = generations_request_body(args);

    let client = create_client();
    let mut request = client
        .post(&url)
        .headers(auth_headers)
        .header("Content-Type", "application/json")
        .header(ADG_TURN_ID_HEADER, turn_id)
        .json(&body)
        .timeout(IMAGE_GENERATION_TIMEOUT);
    if let Some(model) = args.model.as_deref() {
        request = request.header(ADG_IMAGE_MODEL_HEADER, model);
    }

    let response = match request.send().await {
        Ok(response) => response,
        Err(err) => return Ok(transport_failure_outcome(&err)),
    };
    let status = response.status().as_u16();
    let body_bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(err) => return Ok(transport_failure_outcome(&err)),
    };

    if !(200..300).contains(&status) {
        return Ok(map_error_response(status, &body_bytes));
    }

    let Some(b64) = extract_b64_json(&body_bytes) else {
        return Ok(GenerateImageOutcome::Failed {
            code: "no_image_output".to_string(),
            message: "upstream did not return image output".to_string(),
            retryable: false,
        });
    };

    let artifact_call_id = image_generation_artifact_call_id(turn_id, call_id);
    let saved_path = save_image_generation_result(codex_home, session_id, &artifact_call_id, &b64)
        .await
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "generate_image: failed to persist image: {err}"
            ))
        })?;

    Ok(GenerateImageOutcome::Generated {
        saved_path: saved_path.to_string_lossy().into_owned(),
        size: args.size.clone(),
        model: args.model.clone(),
        image_url: format!("data:image/png;base64,{b64}"),
    })
}

fn generations_request_body(args: &GenerateImageArgs) -> JsonValue {
    let mut body = serde_json::Map::new();
    body.insert("prompt".to_string(), JsonValue::String(args.prompt.clone()));
    if let Some(model) = args.model.as_deref() {
        body.insert("model".to_string(), JsonValue::String(model.to_string()));
    }
    if let Some(size) = args.size.as_deref() {
        body.insert("size".to_string(), JsonValue::String(size.to_string()));
    }
    JsonValue::Object(body)
}

/// A transport-level failure (DNS, connect, timeout, body read) never reached a
/// gateway verdict, so it is always technically retryable.
fn transport_failure_outcome(err: &reqwest::Error) -> GenerateImageOutcome {
    let code = if err.is_timeout() {
        "timeout"
    } else {
        "upstream_error"
    };
    GenerateImageOutcome::Failed {
        code: code.to_string(),
        message: format!("generate_image request failed: {err}"),
        retryable: true,
    }
}

fn extract_b64_json(body: &[u8]) -> Option<String> {
    let parsed: JsonValue = serde_json::from_slice(body).ok()?;
    let b64 = parsed
        .get("data")?
        .as_array()?
        .first()?
        .get("b64_json")?
        .as_str()?;
    let trimmed = b64.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Translate a non-2xx gateway response into a §3b outcome. Content/safety
/// refusals are terminal (`refused`, no retry); everything else is `failed`
/// with `retryable` derived from the enumerated code first, then the HTTP class.
fn map_error_response(status: u16, body: &[u8]) -> GenerateImageOutcome {
    let parsed: JsonValue = serde_json::from_slice(body).unwrap_or(JsonValue::Null);
    let (code, message) = extract_error_code_message(&parsed);
    let message = if message.is_empty() {
        format!("image generation failed with status {status}")
    } else {
        message
    };

    if code == "content_policy_violation" || code == "prohibited_content" {
        return GenerateImageOutcome::Refused { code, message };
    }

    let retryable = match code.as_str() {
        // The per-turn breaker is a hard floor; retrying is exactly the runaway
        // it exists to stop.
        "too_many_image_attempts" => false,
        "quota_exceeded" | "rate_limited" | "concurrency_limited" => true,
        // 429 is the rate-limit class, so it is retryable even though it is a
        // 4xx; every other 4xx is a terminal client error.
        _ => matches!(status, 429 | 500..=599),
    };
    let code = if code.is_empty() {
        "upstream_error".to_string()
    } else {
        code
    };
    GenerateImageOutcome::Failed {
        code,
        message,
        retryable,
    }
}

/// ADG error bodies come in two shapes: the gateway breaker emits a flat
/// `{"error":"<code>","message":...}` while forward/moderation paths emit the
/// OpenAI-style nested `{"error":{"code":...,"message":...}}`. Read both.
fn extract_error_code_message(value: &JsonValue) -> (String, String) {
    match value.get("error") {
        Some(JsonValue::String(code)) => {
            let message = value
                .get("message")
                .and_then(JsonValue::as_str)
                .unwrap_or(code)
                .to_string();
            (code.clone(), message)
        }
        Some(JsonValue::Object(error)) => {
            let code = error
                .get("code")
                .and_then(JsonValue::as_str)
                .unwrap_or_default()
                .to_string();
            // Prefer the nested message, but fall back to a sibling top-level
            // `message` so a malformed error object never drops the only text.
            let message = error
                .get("message")
                .or_else(|| value.get("message"))
                .and_then(JsonValue::as_str)
                .unwrap_or_default()
                .to_string();
            (code, message)
        }
        _ => {
            let message = value
                .get("message")
                .and_then(JsonValue::as_str)
                .unwrap_or_default()
                .to_string();
            (String::new(), message)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn function_output_items(item: &ResponseInputItem) -> &Vec<FunctionCallOutputContentItem> {
        match item {
            ResponseInputItem::FunctionCallOutput { output, .. } => match &output.body {
                FunctionCallOutputBody::ContentItems(items) => items,
                FunctionCallOutputBody::Text(_) => panic!("expected content items body"),
            },
            _ => panic!("expected FunctionCallOutput"),
        }
    }

    #[test]
    fn generated_outcome_carries_status_text_and_viewable_image() {
        let out = GenerateImageOutput {
            outcome: GenerateImageOutcome::Generated {
                saved_path: "/tmp/x.png".to_string(),
                size: Some("1024x1024".to_string()),
                model: Some("doubao-seedream".to_string()),
                image_url: "data:image/png;base64,AAA".to_string(),
            },
        };
        let item = out.to_response_item(
            "call_1",
            &ToolPayload::Function {
                arguments: "{}".to_string(),
            },
        );
        let items = function_output_items(&item);
        assert_eq!(items.len(), 2);
        match &items[0] {
            FunctionCallOutputContentItem::InputText { text } => {
                let v: JsonValue = serde_json::from_str(text).unwrap();
                assert_eq!(v["status"], "generated");
                assert_eq!(v["saved_path"], "/tmp/x.png");
            }
            _ => panic!("first item must be the status text"),
        }
        assert!(matches!(
            items[1],
            FunctionCallOutputContentItem::InputImage { .. }
        ));
    }

    #[test]
    fn refused_outcome_is_text_only_and_not_success() {
        let out = GenerateImageOutput {
            outcome: GenerateImageOutcome::Refused {
                code: "content_policy_violation".to_string(),
                message: "blocked".to_string(),
            },
        };
        let item = out.to_response_item(
            "call_1",
            &ToolPayload::Function {
                arguments: "{}".to_string(),
            },
        );
        match &item {
            ResponseInputItem::FunctionCallOutput { output, .. } => {
                assert_eq!(output.success, Some(false));
            }
            _ => panic!("expected FunctionCallOutput"),
        }
        let items = function_output_items(&item);
        assert_eq!(items.len(), 1);
        match &items[0] {
            FunctionCallOutputContentItem::InputText { text } => {
                let v: JsonValue = serde_json::from_str(text).unwrap();
                assert_eq!(v["status"], "refused");
                assert_eq!(v["refusal"]["code"], "content_policy_violation");
            }
            _ => panic!("expected status text"),
        }
    }

    #[test]
    fn extracts_b64_json_from_openai_images_response() {
        let body = br#"{"created":1,"data":[{"b64_json":"AAA"}]}"#;
        assert_eq!(extract_b64_json(body), Some("AAA".to_string()));
    }

    #[test]
    fn missing_or_empty_b64_json_is_no_image() {
        assert_eq!(extract_b64_json(br#"{"data":[{"url":"http://x"}]}"#), None);
        assert_eq!(extract_b64_json(br#"{"data":[{"b64_json":"  "}]}"#), None);
        assert_eq!(extract_b64_json(b"not json"), None);
    }

    #[test]
    fn moderation_refusal_maps_to_terminal_refused() {
        let body = br#"{"error":{"type":"invalid_request_error","code":"content_policy_violation","message":"blocked"}}"#;
        match map_error_response(400, body) {
            GenerateImageOutcome::Refused { code, message } => {
                assert_eq!(code, "content_policy_violation");
                assert_eq!(message, "blocked");
            }
            other => panic!("expected refused, got {:?}", other.status_json()),
        }
    }

    #[test]
    fn breaker_flat_error_is_failed_and_not_retryable() {
        // The gateway breaker returns a flat `{"error":"<code>"}` with HTTP 429,
        // but too_many_image_attempts is a hard floor: retrying is forbidden.
        let body = br#"{"error":"too_many_image_attempts","message":"per-turn cap reached"}"#;
        match map_error_response(429, body) {
            GenerateImageOutcome::Failed {
                code,
                message,
                retryable,
            } => {
                assert_eq!(code, "too_many_image_attempts");
                assert_eq!(message, "per-turn cap reached");
                assert!(!retryable);
            }
            other => panic!("expected failed, got {:?}", other.status_json()),
        }
    }

    #[test]
    fn nested_route_error_passes_code_through_as_non_retryable() {
        let body = br#"{"error":{"code":"image_model_route_unavailable","message":"no route"}}"#;
        match map_error_response(409, body) {
            GenerateImageOutcome::Failed {
                code, retryable, ..
            } => {
                assert_eq!(code, "image_model_route_unavailable");
                assert!(!retryable);
            }
            other => panic!("expected failed, got {:?}", other.status_json()),
        }
    }

    #[test]
    fn server_error_without_code_falls_back_to_retryable_upstream_error() {
        match map_error_response(503, b"upstream exploded") {
            GenerateImageOutcome::Failed {
                code, retryable, ..
            } => {
                assert_eq!(code, "upstream_error");
                assert!(retryable);
            }
            other => panic!("expected failed, got {:?}", other.status_json()),
        }
    }

    #[test]
    fn nested_error_without_message_falls_back_to_top_level_message() {
        let body = br#"{"error":{"code":"upstream_error"},"message":"boom"}"#;
        match map_error_response(500, body) {
            GenerateImageOutcome::Failed { code, message, .. } => {
                assert_eq!(code, "upstream_error");
                assert_eq!(message, "boom");
            }
            other => panic!("expected failed, got {:?}", other.status_json()),
        }
    }

    #[test]
    fn rate_limited_is_retryable() {
        let body = br#"{"error":{"code":"rate_limited","message":"slow down"}}"#;
        match map_error_response(429, body) {
            GenerateImageOutcome::Failed { retryable, .. } => assert!(retryable),
            other => panic!("expected failed, got {:?}", other.status_json()),
        }
    }

    #[test]
    fn generations_body_omits_absent_optional_fields() {
        let args = GenerateImageArgs {
            prompt: "a cat".to_string(),
            size: None,
            model: None,
            reference_image_paths: None,
        };
        let body = generations_request_body(&args);
        assert_eq!(body["prompt"], "a cat");
        assert!(body.get("model").is_none());
        assert!(body.get("size").is_none());
    }

    use codex_utils_absolute_path::test_support::PathExt;
    use http::HeaderMap;
    use http::HeaderValue;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::body_partial_json;
    use wiremock::matchers::header;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    fn bearer_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer sk-adg_test"),
        );
        headers
    }

    // End-to-end over a real HTTP round trip (wiremock): a 200 with b64_json must
    // produce the exact authenticated request, persist the decoded bytes, and
    // return a Generated outcome with a viewable data-URL. This exercises the
    // request building, response parsing, and image save together — not just the
    // pure helpers.
    #[tokio::test]
    async fn generations_success_sends_authed_request_saves_image_and_returns_generated() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/images/generations"))
            .and(header("authorization", "Bearer sk-adg_test"))
            .and(header("x-adg-turn-id", "turn-xyz"))
            .and(header("x-adg-image-model", "doubao-seedream"))
            .and(body_partial_json(
                json!({ "prompt": "a gundam", "model": "doubao-seedream", "size": "1024x1024" }),
            ))
            // "Zm9v" decodes to b"foo"; the save path writes the decoded bytes.
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "data": [{ "b64_json": "Zm9v" }] })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let codex_home = tempfile::tempdir().expect("codex home");
        let codex_home = codex_home.path().abs();
        let args = GenerateImageArgs {
            prompt: "a gundam".to_string(),
            size: Some("1024x1024".to_string()),
            model: Some("doubao-seedream".to_string()),
            reference_image_paths: None,
        };

        let outcome = fulfil_via_generations(
            &format!("{}/v1", server.uri()),
            bearer_headers(),
            "turn-xyz",
            &codex_home,
            "session-1",
            "call-1",
            &args,
        )
        .await
        .expect("fulfilment should not error on a writable codex_home");

        match outcome {
            GenerateImageOutcome::Generated {
                saved_path,
                image_url,
                model,
                ..
            } => {
                assert_eq!(image_url, "data:image/png;base64,Zm9v");
                assert_eq!(model.as_deref(), Some("doubao-seedream"));
                let bytes = std::fs::read(&saved_path).expect("saved image exists");
                assert_eq!(bytes, b"foo");
            }
            other => panic!("expected generated, got {:?}", other.status_json()),
        }
    }

    // A real 400 with a content-policy body must map to a terminal Refused
    // outcome over the wire (not a tool error, not a retry).
    #[tokio::test]
    async fn generations_content_policy_4xx_maps_to_refused() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/images/generations"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "error": { "code": "content_policy_violation", "message": "blocked" }
            })))
            .mount(&server)
            .await;

        let codex_home = tempfile::tempdir().expect("codex home");
        let codex_home = codex_home.path().abs();
        let args = GenerateImageArgs {
            prompt: "x".to_string(),
            size: None,
            model: None,
            reference_image_paths: None,
        };

        let outcome = fulfil_via_generations(
            &format!("{}/v1", server.uri()),
            bearer_headers(),
            "turn-1",
            &codex_home,
            "session-1",
            "call-1",
            &args,
        )
        .await
        .expect("4xx is an outcome, not an error");

        match outcome {
            GenerateImageOutcome::Refused { code, message } => {
                assert_eq!(code, "content_policy_violation");
                assert_eq!(message, "blocked");
            }
            other => panic!("expected refused, got {:?}", other.status_json()),
        }
    }

    // A 200 whose body carries no b64 image must surface no_image_output rather
    // than a fake success.
    #[tokio::test]
    async fn generations_200_without_image_maps_to_no_image_output() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/images/generations"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "data": [{ "url": "http://x" }] })),
            )
            .mount(&server)
            .await;

        let codex_home = tempfile::tempdir().expect("codex home");
        let codex_home = codex_home.path().abs();
        let args = GenerateImageArgs {
            prompt: "x".to_string(),
            size: None,
            model: None,
            reference_image_paths: None,
        };

        let outcome = fulfil_via_generations(
            &format!("{}/v1", server.uri()),
            bearer_headers(),
            "turn-1",
            &codex_home,
            "session-1",
            "call-1",
            &args,
        )
        .await
        .expect("no-image is an outcome, not an error");

        match outcome {
            GenerateImageOutcome::Failed { code, .. } => assert_eq!(code, "no_image_output"),
            other => panic!("expected failed, got {:?}", other.status_json()),
        }
    }
}
