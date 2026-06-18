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
/// Lowercase because `HeaderName::from_static` requires it; HTTP header names
/// are case-insensitive on the wire.
const ADG_TURN_ID_HEADER: &str = "x-adg-turn-id";
/// Request-scoped image model channel (§2): the chosen image model rides this
/// header. The provider config (set by the client from the user's frontend
/// selection) carries a default value; an explicit `model` arg overrides it.
const ADG_IMAGE_MODEL_HEADER: &str = "x-adg-image-model";
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

    // Base headers = auth + the provider's configured headers. The latter is how
    // the client injects the user's frontend image-model selection: it sets
    // X-ADG-Image-Model on the provider (static or env-backed), and an explicit
    // `model` arg later overrides it. See ai-development-gateway
    // docs/generate-image-function-tool-2026-06-17.zh.md §2/§7 (model is
    // request-scoped — the gateway has no tenant default).
    let mut base_headers = auth.to_auth_headers();
    let provider_headers = turn.provider.info().build_header_map().map_err(|err| {
        FunctionCallError::RespondToModel(format!(
            "generate_image: provider headers are invalid: {err}"
        ))
    })?;
    for (name, value) in provider_headers.iter() {
        base_headers.insert(name.clone(), value.clone());
    }

    // Resolve the image model: an explicit tool arg wins, otherwise fall back to
    // the provider's configured default (the client's frontend selection, set as
    // the x-adg-image-model header). The JSON generations path resolves the
    // model from the request *body*, so the resolved value must land there — the
    // header alone is only read on the multipart edits path (§2).
    let image_model = resolve_image_model(args.model.as_deref(), &provider_headers);
    let session_id = invocation.session.conversation_id.to_string();

    // Reference images turn this into an edit: route to the multipart
    // `/v1/images/edits` endpoint. Text-only stays on `/v1/images/generations`.
    let reference_paths = args
        .reference_image_paths
        .as_deref()
        .unwrap_or_default();
    if reference_paths.is_empty() {
        fulfil_via_generations(
            &base_url,
            base_headers,
            &turn.sub_id,
            image_model.as_deref(),
            &turn.config.codex_home,
            &session_id,
            &invocation.call_id,
            args,
        )
        .await
    } else {
        fulfil_via_edits(
            &base_url,
            base_headers,
            &turn.sub_id,
            image_model.as_deref(),
            &turn.config.codex_home,
            &session_id,
            &invocation.call_id,
            args,
            reference_paths,
        )
        .await
    }
}

/// An explicit tool arg wins; otherwise fall back to the provider's configured
/// `x-adg-image-model` default (the client's frontend selection).
fn resolve_image_model(
    arg_model: Option<&str>,
    provider_headers: &http::HeaderMap,
) -> Option<String> {
    arg_model
        .map(str::to_string)
        .filter(|model| !model.trim().is_empty())
        .or_else(|| {
            provider_headers
                .get(ADG_IMAGE_MODEL_HEADER)
                .and_then(|value| value.to_str().ok())
                .map(str::trim)
                .filter(|model| !model.is_empty())
                .map(str::to_string)
        })
}

/// Final request headers: start from the base (auth + provider-configured
/// headers, which may carry the client's default `x-adg-image-model`), then add
/// content-type and the per-turn id, and let an explicit `model_override`
/// replace the provider default. `insert` (not append) makes the override win.
fn finalize_image_request_headers(
    mut headers: http::HeaderMap,
    turn_id: &str,
    model_override: Option<&str>,
) -> http::HeaderMap {
    headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    if let Ok(value) = http::HeaderValue::from_str(turn_id) {
        headers.insert(http::HeaderName::from_static(ADG_TURN_ID_HEADER), value);
    }
    if let Some(model) = model_override
        && let Ok(value) = http::HeaderValue::from_str(model)
    {
        headers.insert(http::HeaderName::from_static(ADG_IMAGE_MODEL_HEADER), value);
    }
    headers
}

/// The networking core, decoupled from `TurnContext` so it can be exercised
/// end-to-end against a local mock server. Sends the authenticated POST, then
/// maps status/body to a §3b outcome; `Err` only on an unwritable codex_home.
async fn fulfil_via_generations(
    base_url: &str,
    base_headers: http::HeaderMap,
    turn_id: &str,
    image_model: Option<&str>,
    codex_home: &AbsolutePathBuf,
    session_id: &str,
    call_id: &str,
    args: &GenerateImageArgs,
) -> Result<GenerateImageOutcome, FunctionCallError> {
    let url = format!("{}/images/generations", base_url.trim_end_matches('/'));
    let body = generations_request_body(&args.prompt, image_model, args.size.as_deref());
    let headers = finalize_image_request_headers(base_headers, turn_id, image_model);

    let client = create_client();
    let request = client
        .post(&url)
        .headers(headers)
        .json(&body)
        .timeout(IMAGE_GENERATION_TIMEOUT);

    complete_images_response(
        request.send().await,
        codex_home,
        session_id,
        turn_id,
        call_id,
        args.size.clone(),
        image_model,
    )
    .await
}

/// Reference-image edit: multipart POST to `/v1/images/edits` carrying the
/// prompt plus each reference image. supply-core already implements this
/// endpoint and returns the same response shape as generations, so the outcome
/// handling is shared via `complete_images_response`.
#[expect(clippy::too_many_arguments)]
async fn fulfil_via_edits(
    base_url: &str,
    base_headers: http::HeaderMap,
    turn_id: &str,
    image_model: Option<&str>,
    codex_home: &AbsolutePathBuf,
    session_id: &str,
    call_id: &str,
    args: &GenerateImageArgs,
    reference_paths: &[String],
) -> Result<GenerateImageOutcome, FunctionCallError> {
    let url = format!("{}/images/edits", base_url.trim_end_matches('/'));
    let form =
        match build_image_edit_form(&args.prompt, image_model, args.size.as_deref(), reference_paths)
            .await
        {
            Ok(form) => form,
            Err(outcome) => return Ok(outcome),
        };

    // Multipart sets its own Content-Type (with boundary), so unlike the JSON
    // generations path we must not stamp application/json. The image model still
    // rides the X-ADG-Image-Model header so the gateway resolves the edits route.
    let headers = finalize_image_edit_headers(base_headers, turn_id, image_model);

    let client = create_client();
    let request = client
        .post(&url)
        .headers(headers)
        .multipart(form)
        .timeout(IMAGE_GENERATION_TIMEOUT);

    complete_images_response(
        request.send().await,
        codex_home,
        session_id,
        turn_id,
        call_id,
        args.size.clone(),
        image_model,
    )
    .await
}

/// Shared post-send handling for both image endpoints: map transport / non-2xx
/// failures to §3b outcomes, extract the b64 image, and persist it. Returns
/// `Err` only on an unwritable codex_home.
async fn complete_images_response(
    send_result: Result<reqwest::Response, reqwest::Error>,
    codex_home: &AbsolutePathBuf,
    session_id: &str,
    turn_id: &str,
    call_id: &str,
    size: Option<String>,
    image_model: Option<&str>,
) -> Result<GenerateImageOutcome, FunctionCallError> {
    let response = match send_result {
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
        size,
        model: image_model.map(str::to_string),
        image_url: format!("data:image/png;base64,{b64}"),
    })
}

/// Build the multipart form supply-core's `/v1/images/edits` parser expects:
/// text fields (`prompt` / `model` / `size` / `response_format`) plus one
/// `image` part per reference. An unreadable reference is a terminal `failed`
/// outcome — the model supplied a bad path — rather than a hard error, so the
/// model can correct the path or fall back to a prompt-only description.
async fn build_image_edit_form(
    prompt: &str,
    model: Option<&str>,
    size: Option<&str>,
    reference_paths: &[String],
) -> Result<reqwest::multipart::Form, GenerateImageOutcome> {
    let mut form = reqwest::multipart::Form::new()
        .text("prompt", prompt.to_string())
        .text("response_format", "b64_json");
    if let Some(model) = model {
        form = form.text("model", model.to_string());
    }
    if let Some(size) = size {
        form = form.text("size", size.to_string());
    }
    for path in reference_paths {
        let bytes = match tokio::fs::read(path).await {
            Ok(bytes) => bytes,
            Err(err) => {
                return Err(GenerateImageOutcome::Failed {
                    code: "reference_image_unreadable".to_string(),
                    message: format!("could not read reference image {path}: {err}"),
                    retryable: false,
                });
            }
        };
        let file_name = std::path::Path::new(path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("image")
            .to_string();
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(file_name)
            .mime_str(image_part_mime(path))
            .map_err(|err| GenerateImageOutcome::Failed {
                code: "reference_image_invalid".to_string(),
                message: format!("invalid reference image {path}: {err}"),
                retryable: false,
            })?;
        form = form.part("image", part);
    }
    Ok(form)
}

/// Like `finalize_image_request_headers` but for the multipart edits path:
/// carries the per-turn id and the image-model header, and deliberately omits
/// Content-Type — reqwest's multipart layer sets it, including the boundary.
fn finalize_image_edit_headers(
    mut headers: http::HeaderMap,
    turn_id: &str,
    model_override: Option<&str>,
) -> http::HeaderMap {
    if let Ok(value) = http::HeaderValue::from_str(turn_id) {
        headers.insert(http::HeaderName::from_static(ADG_TURN_ID_HEADER), value);
    }
    if let Some(model) = model_override
        && let Ok(value) = http::HeaderValue::from_str(model)
    {
        headers.insert(http::HeaderName::from_static(ADG_IMAGE_MODEL_HEADER), value);
    }
    headers
}

/// Map a reference path's extension to the image MIME the providers accept.
/// Defaults to PNG (the dominant case and a safe fallback).
fn image_part_mime(path: &str) -> &'static str {
    match std::path::Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        _ => "image/png",
    }
}

fn generations_request_body(prompt: &str, model: Option<&str>, size: Option<&str>) -> JsonValue {
    let mut body = serde_json::Map::new();
    body.insert("prompt".to_string(), JsonValue::String(prompt.to_string()));
    if let Some(model) = model {
        body.insert("model".to_string(), JsonValue::String(model.to_string()));
    }
    if let Some(size) = size {
        body.insert("size".to_string(), JsonValue::String(size.to_string()));
    }
    // Force inline base64. Verified on UAT: without it doubao-seedream returns a
    // (signed, expiring) `url` instead of `b64_json`, which our save path can't
    // consume; gpt-image returns b64 either way and accepts the param. So
    // requesting b64_json makes the response shape uniform across providers.
    body.insert(
        "response_format".to_string(),
        JsonValue::String("b64_json".to_string()),
    );
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
        let body = generations_request_body(&args.prompt, None, None);
        assert_eq!(body["prompt"], "a cat");
        assert!(body.get("model").is_none());
        assert!(body.get("size").is_none());
        // response_format is always forced so providers return inline b64.
        assert_eq!(body["response_format"], "b64_json");
    }

    #[test]
    fn resolve_image_model_prefers_arg_then_provider_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::HeaderName::from_static("x-adg-image-model"),
            HeaderValue::from_static("frontend-selected"),
        );
        // Explicit arg wins.
        assert_eq!(
            resolve_image_model(Some("tool-arg"), &headers).as_deref(),
            Some("tool-arg")
        );
        // No arg -> provider default (the client's frontend selection).
        assert_eq!(
            resolve_image_model(None, &headers).as_deref(),
            Some("frontend-selected")
        );
        // Neither -> None (the call will fail-fast at the gateway).
        assert_eq!(resolve_image_model(None, &HeaderMap::new()), None);
        // Blank arg is ignored, falls through to the provider default.
        assert_eq!(
            resolve_image_model(Some("  "), &headers).as_deref(),
            Some("frontend-selected")
        );
    }

    #[test]
    fn explicit_model_overrides_provider_default_image_header() {
        let mut base = HeaderMap::new();
        base.insert(
            http::HeaderName::from_static("x-adg-image-model"),
            HeaderValue::from_static("provider-default-model"),
        );
        // No override -> the provider default (the user's frontend selection)
        // is preserved.
        let kept = finalize_image_request_headers(base.clone(), "turn-1", None);
        assert_eq!(kept["x-adg-image-model"], "provider-default-model");
        assert_eq!(kept["x-adg-turn-id"], "turn-1");
        assert_eq!(kept[http::header::CONTENT_TYPE], "application/json");
        // Explicit arg -> replaces (not appends to) the provider default.
        let overridden =
            finalize_image_request_headers(base, "turn-1", Some("model-from-tool-arg"));
        let values: Vec<_> = overridden
            .get_all("x-adg-image-model")
            .iter()
            .map(|v| v.to_str().unwrap().to_string())
            .collect();
        assert_eq!(values, vec!["model-from-tool-arg"]);
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
            .and(body_partial_json(json!({
                "prompt": "a gundam",
                "model": "doubao-seedream",
                "size": "1024x1024",
                "response_format": "b64_json",
            })))
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
            Some("doubao-seedream"),
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

    // The frontend-selection path: when the tool arg omits `model`, the
    // provider-configured X-ADG-Image-Model (set by the client from the user's
    // selection) must reach the gateway over the wire.
    #[tokio::test]
    async fn provider_default_image_model_is_sent_when_arg_omits_model() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/images/generations"))
            // The resolved image model must reach the gateway in BOTH the header
            // and the JSON body (the generations path routes on the body model).
            .and(header("x-adg-image-model", "frontend-selected-model"))
            .and(body_partial_json(
                json!({ "model": "frontend-selected-model" }),
            ))
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
            prompt: "a cat".to_string(),
            size: None,
            model: None, // not specified by the tool call; resolved from provider default
            reference_image_paths: None,
        };

        let outcome = fulfil_via_generations(
            &format!("{}/v1", server.uri()),
            bearer_headers(),
            "turn-xyz",
            Some("frontend-selected-model"),
            &codex_home,
            "session-1",
            "call-1",
            &args,
        )
        .await
        .expect("fulfilment should not error");
        assert!(matches!(outcome, GenerateImageOutcome::Generated { .. }));
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
            None,
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
            None,
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

    // Reference images switch to the multipart /v1/images/edits endpoint. The
    // request must carry auth + per-turn + image-model headers and a
    // multipart/form-data body, and a 200 persists the decoded image exactly
    // like generations (shared via complete_images_response).
    #[tokio::test]
    async fn edits_with_reference_image_sends_multipart_and_returns_generated() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/images/edits"))
            .and(header("authorization", "Bearer sk-adg_test"))
            .and(header("x-adg-turn-id", "turn-edit"))
            .and(header("x-adg-image-model", "gpt-image"))
            .and(wiremock::matchers::header_regex(
                "content-type",
                "^multipart/form-data",
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "data": [{ "b64_json": "Zm9v" }] })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let workdir = tempfile::tempdir().expect("workdir");
        let ref_path = workdir.path().join("ref.png");
        std::fs::write(&ref_path, b"original-bytes").expect("write reference");
        let codex_home = tempfile::tempdir().expect("codex home");
        let codex_home = codex_home.path().abs();
        let args = GenerateImageArgs {
            prompt: "change the outfit".to_string(),
            size: None,
            model: Some("gpt-image".to_string()),
            reference_image_paths: Some(vec![ref_path.to_string_lossy().into_owned()]),
        };

        let outcome = fulfil_via_edits(
            &format!("{}/v1", server.uri()),
            bearer_headers(),
            "turn-edit",
            Some("gpt-image"),
            &codex_home,
            "session-1",
            "call-1",
            &args,
            args.reference_image_paths.as_deref().unwrap(),
        )
        .await
        .expect("edit fulfilment should not error on a writable codex_home");

        match outcome {
            GenerateImageOutcome::Generated { saved_path, .. } => {
                let bytes = std::fs::read(&saved_path).expect("saved image exists");
                assert_eq!(bytes, b"foo");
            }
            other => panic!("expected generated, got {:?}", other.status_json()),
        }
    }

    // A reference path that cannot be read is a terminal `failed` outcome (the
    // model passed a bad path) and never reaches the network.
    #[tokio::test]
    async fn edits_with_unreadable_reference_returns_failed_without_request() {
        let codex_home = tempfile::tempdir().expect("codex home");
        let codex_home = codex_home.path().abs();
        let args = GenerateImageArgs {
            prompt: "change the outfit".to_string(),
            size: None,
            model: Some("gpt-image".to_string()),
            reference_image_paths: Some(vec!["/nonexistent/does-not-exist.png".to_string()]),
        };

        let outcome = fulfil_via_edits(
            // Unreachable base_url: the form build fails first, so nothing is sent.
            "http://127.0.0.1:1/v1",
            bearer_headers(),
            "turn-edit",
            Some("gpt-image"),
            &codex_home,
            "session-1",
            "call-1",
            &args,
            args.reference_image_paths.as_deref().unwrap(),
        )
        .await
        .expect("unreadable reference is an outcome, not an error");

        match outcome {
            GenerateImageOutcome::Failed {
                code, retryable, ..
            } => {
                assert_eq!(code, "reference_image_unreadable");
                assert!(!retryable);
            }
            other => panic!("expected failed, got {:?}", other.status_json()),
        }
    }
}
