# Codex architecture overview

This note is a map for secondary development. It describes the current
repository shape, the major runtime layers, and the most useful extension
points before making deeper changes.

## Repository shape

Codex is primarily a Rust monorepo. The npm package is a distribution wrapper:
it selects the platform-specific native binary and forwards CLI arguments to it.

```text
codex-cli/bin/codex.js
  -> native codex binary
    -> codex-rs/cli
      -> tui / exec / app-server / mcp-server
        -> app-server-client
          -> codex-core
            -> Session / Turn / ModelClient / ToolRouter
              -> shell / apply_patch / MCP / multi-agent / web-search tools
          -> codex-api
            -> OpenAI Responses API over WebSocket or HTTP SSE
```

The main Rust workspace lives under `codex-rs/`. The most important crates are:

- `cli`: the top-level `codex` multitool.
- `tui`: the interactive terminal UI.
- `exec`: the non-interactive automation entrypoint.
- `app-server`: the JSON-RPC server used by richer clients such as IDEs.
- `app-server-protocol`: the app-server wire types.
- `app-server-client`: shared in-process client/runtime wiring used by CLI
  surfaces.
- `core`: the agent runtime and business logic.
- `codex-api`: typed OpenAI/Codex API clients.
- `protocol`: internal protocol types shared by core and UI layers.
- `tools`: model-visible tool schemas and registry planning helpers.
- `sandboxing`: platform sandbox selection and policy enforcement support.

## Entry points

The npm binary entrypoint is `codex-cli/bin/codex.js`. It detects the host
platform, finds the matching native binary, adjusts `PATH` for bundled helpers,
and then spawns the Rust executable.

The Rust CLI entrypoint is `codex-rs/cli/src/main.rs`. It defines the top-level
`codex` command. Without a subcommand, options are forwarded to the interactive
TUI. With a subcommand, it dispatches to features such as:

- `codex exec`: run Codex non-interactively.
- `codex review`: run a non-interactive code review.
- `codex app-server`: start the JSON-RPC app server.
- `codex mcp-server`: expose Codex as an MCP server.
- `codex mcp`: manage MCP server launchers in config.
- `codex sandbox`: run a command under a Codex sandbox.
- `codex resume` / `codex fork`: continue or branch stored sessions.
- `codex login` / `codex logout`: manage authentication.

## Core runtime

The core runtime lives in `codex-rs/core`.

`Session` is the long-lived agent state for a thread. It owns conversation
identity, configuration, active turn state, MCP connection state, network proxy
state, services, event channels, and persisted rollout/thread metadata.

`run_turn` in `codex-rs/core/src/session/turn.rs` is the main agent loop. At a
high level it:

1. Loads turn-scoped configuration and context.
2. Handles pre-sampling compaction when history is too large.
3. Resolves skills, plugin mentions, app/connector mentions, MCP tools, and
   dynamic tools.
4. Builds the model prompt and visible tool specs.
5. Streams model output from the provider.
6. Converts model tool calls into internal tool invocations.
7. Executes tool calls and appends tool outputs back into the next model
   request.
8. Completes when the model produces the final assistant message or the turn is
   interrupted.

`ModelClient` in `codex-rs/core/src/client.rs` is the model-provider boundary.
It owns session-scoped provider/auth state, request headers, transport fallback
state, and response stream mapping. A `ModelClientSession` is created per turn
and may reuse a Responses WebSocket connection inside that turn. It also tracks
the `x-codex-turn-state` sticky-routing token and can fall back to HTTP SSE.

## Tool system

Model-visible tools flow through a dedicated routing and orchestration layer.

```text
model ResponseItem tool call
  -> ToolRouter::build_tool_call
  -> ToolRegistry handler lookup
  -> ToolOrchestrator
       approval decision
       sandbox selection
       network approval handling
       first attempt
       retry/escalation on sandbox denial when allowed
  -> ToolOutput
  -> ResponseInputItem returned to the model
```

Key files:

- `codex-rs/core/src/tools/router.rs`: converts model response items into
  internal tool calls and exposes model-visible tool specs.
- `codex-rs/core/src/tools/registry.rs`: registers concrete tool handlers and
  dispatches invocations.
- `codex-rs/core/src/tools/orchestrator.rs`: central approval, sandbox, network
  approval, retry, and escalation logic.
- `codex-rs/core/src/tools/handlers/shell.rs`: shell and local shell handlers.
- `codex-rs/core/src/tools/runtimes/shell.rs`: shell execution runtime.
- `codex-rs/core/src/tools/handlers/apply_patch.rs`: patch parsing,
  permission assessment, streaming patch progress, and handler glue.
- `codex-rs/core/src/tools/runtimes/apply_patch.rs`: verified patch execution
  under the selected filesystem sandbox context.
- `codex-rs/core/src/tools/handlers/mcp.rs`: MCP tool calls.
- `codex-rs/core/src/tools/handlers/multi_agents*`: subagent tooling.

This is the primary extension area for adding first-party tools or changing how
tool approval and sandboxing behave.

## App server

`codex app-server` exposes Codex over a JSON-RPC protocol. It is the interface
used by richer clients such as IDE integrations and desktop-style frontends.

The core protocol resources are:

- `Thread`: a conversation between a user and Codex.
- `Turn`: one user-to-agent interaction within a thread.
- `Item`: a persisted unit inside a turn, such as user messages, reasoning,
  assistant messages, shell calls, file edits, or tool results.

Important app-server files:

- `codex-rs/app-server/README.md`: protocol and lifecycle documentation.
- `codex-rs/app-server/src/message_processor.rs`: initialized JSON-RPC request
  dispatch.
- `codex-rs/app-server/src/request_processors`: request-specific business
  logic.
- `codex-rs/app-server-protocol/src/protocol/v2.rs`: v2 request, response, and
  notification types.

Common API methods include `initialize`, `thread/start`, `thread/resume`,
`thread/fork`, `turn/start`, `turn/steer`, `turn/interrupt`, `review/start`,
`model/list`, `skills/list`, `plugin/list`, `app/list`, `mcpServer/tool/call`,
and `config/read`.

## UI surfaces

Codex has several user-facing surfaces that share the same runtime concepts:

- `codex-rs/tui`: the fullscreen Ratatui terminal UI.
- `codex-rs/exec`: the non-interactive CLI surface. Its stdout contract is
  intentionally strict: final output or JSONL goes to stdout, diagnostics go to
  stderr.
- `codex-rs/app-server`: external clients and IDEs.
- `codex-rs/mcp-server`: exposes Codex as an MCP tool to other agents.

For new product surfaces, prefer app-server integration over coupling directly
to `codex-core`. That keeps the client on a stable JSON-RPC boundary and avoids
duplicating lifecycle, thread, and approval behavior.

## Configuration, permissions, and sandboxing

User configuration is TOML-based. The effective config is assembled from config
files, CLI overrides, managed requirements, feature flags, and runtime updates.

Important files:

- `codex-rs/core/src/config/mod.rs`: effective runtime config.
- `codex-rs/config/src/config_toml.rs`: config file schema.
- `codex-rs/config/src/config_requirements.rs`: managed requirements and
  constraints.
- `codex-rs/sandboxing`: platform sandbox support and policy transforms.

Codex supports legacy sandbox modes such as `read-only`, `workspace-write`, and
`danger-full-access`, plus newer permission profiles that can express more
precise filesystem and network permissions. Platform enforcement differs:

- macOS uses Seatbelt.
- Linux uses bubblewrap, with legacy Landlock compatibility where possible.
- Windows has dedicated elevated and restricted-token sandbox paths.

## MCP, plugins, and skills

Codex can connect to MCP servers as a client and can also run as an MCP server.
MCP tools are discovered and exposed through the same tool routing layer as
native tools.

Plugins and skills provide higher-level capability packaging:

- Skills are markdown-guided workflows and instructions loaded into turns when
  mentioned or otherwise selected.
- Plugins can bundle skills, apps/connectors, and MCP server definitions.
- App/connector availability is merged from plugin configuration and accessible
  MCP tools.

Important files and areas:

- `codex-rs/core/src/session/mcp.rs`: session-level MCP refresh and tool info
  lookup.
- `codex-rs/core/src/mcp_tool_exposure.rs`: model-visible MCP exposure
  planning.
- `codex-rs/core/src/mcp_skill_dependencies.rs`: skill-declared MCP
  dependency prompting and installation.
- `codex-rs/core/src/skills.rs`: skill loading and rendering.
- `codex-rs/core/src/plugins`: plugin discovery and turn injection support.
- `codex-rs/codex-mcp`: MCP connection management.

## Useful secondary-development paths

For custom development, start from the smallest stable boundary that matches
the goal:

- Custom API/provider routing: inspect `codex-rs/core/src/client.rs`,
  `codex-rs/codex-api`, `codex-rs/model-provider`, and
  `codex-rs/model-provider-info`.
- New model-visible tool: add schema planning in the tools layer, then a
  handler/runtime under `codex-rs/core/src/tools`.
- Custom product UI: build against `codex app-server` instead of calling
  `codex-core` directly.
- Prompt/personality/collaboration behavior: inspect `codex-rs/core/src/context`
  and `codex-rs/collaboration-mode-templates`.
- Sandbox or approval behavior: inspect `codex-rs/core/src/tools/orchestrator.rs`
  and `codex-rs/sandboxing`.
- TUI changes: inspect `codex-rs/tui/src/app.rs`,
  `codex-rs/tui/src/chatwidget`, and `codex-rs/tui/src/bottom_pane`.

## Development hygiene

When changing Rust code in `codex-rs`, follow the repository instructions in
`AGENTS.md`:

- Prefer small crates/modules over growing `codex-core`.
- Run `just fmt` after Rust changes.
- Run the relevant crate tests, for example `cargo test -p codex-tui` or
  `cargo test -p codex-core`.
- For app-server protocol changes, update protocol docs/schema fixtures and run
  `cargo test -p codex-app-server-protocol`.
- For user-visible TUI changes, update `insta` snapshots.

