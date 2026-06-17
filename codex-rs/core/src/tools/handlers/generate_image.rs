use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseInputItem;
use serde::Deserialize;
use serde_json::Value as JsonValue;
use serde_json::json;

use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;
use codex_tools::GENERATE_IMAGE_TOOL_NAME;
use codex_tools::ToolName;

pub struct GenerateImageHandler;

#[derive(Deserialize)]
#[allow(dead_code)] // fields consumed once the B2b fulfilment path lands.
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
        let arguments = match invocation.payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "generate_image handler received unsupported payload".to_string(),
                ));
            }
        };
        let _args: GenerateImageArgs = parse_arguments(&arguments)?;
        // TODO(B2b): fulfil via an authenticated POST to /v1/images on the
        // provider base_url (reuse AuthProvider::add_auth_headers), carry
        // X-ADG-Turn-Id, translate the HTTP outcome into GenerateImageOutcome,
        // save the image, and return a viewable result. The tool is not yet
        // registered in the tool plan, so this path is unreachable today.
        Err(FunctionCallError::RespondToModel(
            "generate_image is not yet available".to_string(),
        ))
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
}
