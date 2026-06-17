use crate::JsonSchema;
use crate::ResponsesApiTool;
use crate::ToolSpec;
use std::collections::BTreeMap;

/// Name of the converged image-generation function tool. Unlike the native
/// `image_generation` built-in (which Volcengine Ark's /responses rejects), this
/// is a plain `function` tool the model can call on any provider; the gateway
/// fulfils it via /v1/images and returns a structured result the model reads to
/// decide whether to stop. See ai-development-gateway
/// docs/generate-image-function-tool-2026-06-17.zh.md §1/§3b.
pub const GENERATE_IMAGE_TOOL_NAME: &str = "generate_image";

/// Builds the `generate_image` function-tool spec. Define-first: this is the
/// stable request-side contract; the handler that fulfils calls lands
/// separately. Not yet registered in the tool plan (inert until wired).
pub fn create_generate_image_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "prompt".to_string(),
            JsonSchema::string(Some(
                "Full image prompt describing the desired image.".to_string(),
            )),
        ),
        (
            "size".to_string(),
            // Free-form, not an enum: valid sizes differ per provider and a fixed
            // enum advertised only small sizes that some image models reject.
            // Omitting size lets the provider pick a valid default (verified on
            // doubao + gpt-image), which is the recommended path.
            JsonSchema::string(Some(
                "Optional output image size. Omit to let the provider choose a valid \
default (recommended). Accepted forms vary by model, e.g. WIDTHxHEIGHT, 1k, 2k, 4k, \
or auto; some models require large sizes (>= ~2k)."
                    .to_string(),
            )),
        ),
        (
            "model".to_string(),
            JsonSchema::string(Some(
                "Optional image model alias; omit for the tenant default.".to_string(),
            )),
        ),
        (
            "reference_image_paths".to_string(),
            JsonSchema::array(
                JsonSchema::string(Some(
                    "Local filesystem path to a reference/edit image.".to_string(),
                )),
                Some(
                    "Optional local images to use as edit targets or style references.".to_string(),
                ),
            ),
        ),
    ]);

    ToolSpec::Function(ResponsesApiTool {
        name: GENERATE_IMAGE_TOOL_NAME.to_string(),
        description: "Generate or edit a raster image. Returns a structured result: \
status=generated (saved_path plus a viewable image), status=refused (content/policy \
refusal; message explains why), or status=failed (error.code plus retryable). Issue \
one call per requested asset."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["prompt".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn generate_image_tool_serializes_as_function_with_prompt_required() {
        let spec = create_generate_image_tool();
        let v = serde_json::to_value(&spec).expect("serialize tool spec");
        assert_eq!(v["type"], "function");
        assert_eq!(v["name"], GENERATE_IMAGE_TOOL_NAME);
        assert_eq!(v["parameters"]["type"], "object");
        assert_eq!(v["parameters"]["required"], json!(["prompt"]));
        // size is a free-form optional string (no enum): valid sizes are
        // provider-specific and omitting it picks a valid default.
        assert_eq!(v["parameters"]["properties"]["size"]["type"], "string");
        assert!(v["parameters"]["properties"]["size"].get("enum").is_none());
        // additionalProperties locked off so the model can't smuggle unknown args.
        assert_eq!(v["parameters"]["additionalProperties"], json!(false));
    }
}
