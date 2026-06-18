---
name: "imagegen"
description: "Generate raster images when the task benefits from AI-created bitmap visuals such as photos, illustrations, textures, sprites, mockups, or product shots. Use when Codex should create a brand-new image and the output should be a bitmap asset rather than repo-native code or vector. Do not use when the task is better handled by editing existing SVG/vector/code-native assets, extending an established icon or logo system, or building the visual directly in HTML/CSS/canvas."
---

# Image Generation Skill

Generates images for the current task (for example website assets, game assets, UI mockups, product mockups, logo exploration, photorealistic images, or infographics).

## The tool

Use the **`generate_image`** function tool. It sends the prompt to the configured image gateway and returns a structured result you read to decide what to do next:

- `status: "generated"` — success. `saved_path` is the absolute path of the saved image, and a viewable copy is attached so you can see what you produced. Reference the path and stop.
- `status: "refused"` — the request was refused on content/policy grounds (`refusal.message` explains why). This is terminal: report it to the user and do not retry the same prompt.
- `status: "failed"` — a technical failure (`error.code`, `error.retryable`). If `retryable` is true you may try once more; if false, report it and stop.

Core rules:

- **One call per requested asset or variant.** Do not loop. Issue a single `generate_image` call, read the structured result, and act on it — never re-issue the same request hoping for a different outcome. Distinct assets need distinct calls with distinct prompts.
- **Stop when you have the image.** A `generated` result already includes the saved path and a viewable image; reference the path and finish. Do not regenerate to "double-check".
- **Do not retry a `refused` result**, and do not retry a `failed` result whose `error.retryable` is false.

## Choosing the model

The `model` argument selects the image model:

- **Omit `model`** to use the deployment's default image model (the one the user selected in the client). This is the normal path — for an ordinary "make me an image" request, just omit it.
- **Pass `model`** only when the user explicitly asks for a specific image model, or when a previous call's `failed` result indicates the default is unavailable and you know another available alias.

There is no built-in `OPENAI_API_KEY`, CLI fallback, or local script in this flow — `generate_image` is the only path.

## Size

`size` is optional and free-form. **Omit it** to let the provider pick a valid default — this is the recommended path and works across image models. Only pass `size` when the user asks for a specific dimension; valid forms are provider-specific (for example `2k`, `4k`, `2048x2048`, or `1024x1024`), and some models reject sizes that are too small, so prefer omitting it unless you have a concrete requirement.

## Reference images / editing

To **edit an existing image** (change an outfit, swap a background, restyle a scene) or derive a variant from it, pass `reference_image_paths`: a list of absolute paths to local image files (PNG/JPEG/WebP). The prompt then describes the change to apply to those images rather than a scene to create from scratch.

- Use it whenever the user supplies a source image and wants it modified, or asks to keep a subject/identity while changing something around it ("same person, different outfit").
- Describe what to change in the prompt and what to keep ("keep the same face and pose, change only the jacket to red"); identity/likeness is preserved well but is not pixel-locked.
- Omit `reference_image_paths` for an ordinary text-to-image generation.
- A path that cannot be read returns a terminal `failed` result with code `reference_image_unreadable` — fix the path or fall back to a prompt-only description.

## When to use
- Generate a new image (concept art, product shot, cover, website hero, sprite, texture).
- Generate a photorealistic, illustration, or stylized bitmap asset for the current task.
- Edit an existing local image — change outfit/background/style or derive a variant — via `reference_image_paths`.
- Produce several assets or variants — one `generate_image` call each.

## When not to use
- Extending or matching an existing SVG/vector icon set, logo system, or illustration library inside the repo.
- Creating simple shapes, diagrams, wireframes, or icons better produced directly in SVG, HTML/CSS, or canvas.
- Any task where the user clearly wants deterministic code-native output instead of a generated bitmap.

## Workflow
1. Confirm the task wants a generated bitmap (not vector/code-native output).
2. Collect inputs: the prompt, any exact text to render verbatim, constraints/avoid list, and — if editing an existing image — its absolute path for `reference_image_paths`.
3. Decide model: omit `model` for the default; pass it only on an explicit request or a known-good alternate.
4. Decide size: omit unless the user requires a specific dimension.
5. Shape the prompt (see below). For a generic prompt, add tasteful detail only when it materially improves the result; for a detailed prompt, normalize it without inventing requirements.
6. Issue one `generate_image` call.
7. Read the structured result:
   - `generated` → report the `saved_path`; if the asset is meant for the project, move/copy it from `saved_path` into the workspace and update any consuming code. Never leave a project-referenced asset only at the default path.
   - `refused` → report `refusal.message`; stop.
   - `failed` → report `error.code`; retry once only if `error.retryable` is true.
8. For several assets, repeat one call per asset — do not use a single call to produce unrelated assets.
9. Iterate with a single targeted change when the user asks for a revision, then re-check.
10. Always report the final saved path(s) and the final prompt(s) used.

## Prompt augmentation

Reformat the user's request into a structured, production-oriented spec. Make the goal clearer and more actionable without blindly adding detail.

Specificity policy:
- If the prompt is already specific and detailed, preserve that specificity and only normalize/structure it.
- If the prompt is generic, add tasteful augmentation when it will materially improve the result.

Allowed: composition/framing hints, polish-level or intended-use hints, practical layout guidance, reasonable scene concreteness that supports the request.

Not allowed: extra characters or objects not implied by the request; brand names, slogans, palettes, or narrative beats not implied; arbitrary placement the layout does not support.

## Use-case taxonomy (exact slugs)

Classify each request into one bucket and keep the slug consistent across prompts.

- photorealistic-natural — candid/editorial lifestyle scenes with real texture and natural lighting.
- product-mockup — product/packaging shots, catalog imagery, merch concepts.
- ui-mockup — app/web interface mockups and wireframes; specify the desired fidelity.
- infographic-diagram — diagrams/infographics with structured layout and text.
- scientific-educational — classroom explainers and learning visuals with required labels and accuracy.
- ads-marketing — campaign concepts with audience, brand position, scene, and exact tagline/copy.
- productivity-visual — slide, chart, workflow, and data-heavy business visuals.
- logo-brand — logo/mark exploration.
- illustration-story — comics, children's book art, narrative scenes.
- stylized-concept — style-driven concept art, 3D/stylized renders.
- historical-scene — period-accurate/world-knowledge scenes.

## Shared prompt schema

Use this labeled spec as scaffolding (use only the lines that help):

```text
Use case: <taxonomy slug>
Asset type: <where the asset will be used>
Primary request: <user's main prompt>
Scene/backdrop: <environment>
Subject: <main subject>
Style/medium: <photo/illustration/3D/etc>
Composition/framing: <wide/close/top-down; placement>
Lighting/mood: <lighting + mood>
Color palette: <palette notes>
Materials/textures: <surface details>
Text (verbatim): "<exact text>"
Constraints: <must keep/must avoid>
Avoid: <negative constraints>
```

Augmentation rules:
- Keep it short; add only the details that materially improve the prompt.
- If a critical detail is missing and blocks success, ask a question; otherwise proceed.

## Examples

### Hero image
```text
Use case: product-mockup
Asset type: landing page hero
Primary request: a minimal hero image of a ceramic coffee mug
Style/medium: clean product photography
Composition/framing: wide composition with usable negative space for page copy
Lighting/mood: soft studio lighting
Constraints: no logos, no text, no watermark
```

### Infographic with text
```text
Use case: infographic-diagram
Asset type: blog illustration
Primary request: a simple three-step onboarding diagram
Text (verbatim): "1. Sign up" "2. Connect" "3. Build"
Constraints: legible labels; clean flat style; no watermark
```

## Prompting best practices
- Structure the prompt as scene/backdrop -> subject -> details -> constraints.
- Include intended use (ad, UI mock, infographic) to set the polish level.
- Use camera/composition language for photorealism.
- Quote exact text verbatim and specify typography + placement; for tricky words, spell them letter-by-letter.
- For a revision, change one thing at a time and re-check.
- Only use SVG/vector stand-ins when the user explicitly asked for vector output or a non-image placeholder.
- If the prompt is generic, add only the extra detail that materially helps; if it is already detailed, normalize it instead of expanding it.

More shared prompting principles: `references/prompting.md`.
Copy/paste prompt recipes: `references/sample-prompts.md`.
