# Secondary Development Guide

This document defines how to maintain a private secondary-development fork of
Codex while staying close to the official `openai/codex` repository.

The goal is simple: keep local product changes easy to review, easy to rebase,
and easy to separate from upstream changes.

## Repository Roles

Use two remotes:

```bash
git remote add origin https://github.com/<your-org-or-user>/codex
git remote add upstream https://github.com/openai/codex
```

Remote responsibilities:

- `upstream` points to the official OpenAI repository. Fetch from it only.
- `origin` points to the private fork. Push secondary-development branches here.

Do not push directly to `upstream`.

## Branch Model

Use the following long-lived branches:

- `main`: tracks `upstream/main` as closely as possible.
- `codex/base`: the private integration base for secondary development.
- `codex/secondary-dev`: the current secondary-development integration branch.

Use short-lived branches for each concrete change:

- `codex/feature/<name>` for product features.
- `codex/fix/<name>` for bug fixes.
- `codex/experiment/<name>` for disposable experiments.
- `codex/sync-upstream-<date>` for upstream synchronization work.

Keep `main` clean. Do not put private product changes on `main`.

## Sync Policy

Fetch upstream at the start of each working session:

```bash
git fetch upstream
```

Update `main` with a fast-forward only merge:

```bash
git checkout main
git merge --ff-only upstream/main
git push origin main
```

If the fast-forward fails, stop and inspect the branch. A non-fast-forward
failure means `main` contains commits that are not in official upstream, which
breaks the branch model.

After `main` is updated, update the secondary-development base:

```bash
git checkout codex/base
git merge main
git push origin codex/base
```

Then update active secondary-development branches from `codex/base`.

For branches used by one developer, prefer rebase:

```bash
git checkout codex/feature/<name>
git rebase codex/base
```

For shared branches, prefer merge:

```bash
git checkout codex/secondary-dev
git merge codex/base
git push origin codex/secondary-dev
```

Do not rewrite shared branch history unless every collaborator has agreed.

## Change Isolation Rules

Keep private changes small and isolated. A change is easier to sync when it has a
clear boundary and a narrow set of touched files.

Preferred order of customization:

1. Configuration.
2. Feature flags.
3. New modules with narrow public APIs.
4. New crates when the functionality is large or reusable.
5. Small, well-contained edits to upstream files.
6. Broad upstream-file rewrites only when there is no practical alternative.

Avoid mixing upstream sync, formatting churn, refactors, and product changes in
one commit.

Each commit should answer one question: what changed, and why?

## Codex Rust Rules

For Rust changes under `codex-rs/`, follow the repository rules in
`AGENTS.md`.

Important defaults:

- Prefer crates and modules outside `codex-core` when adding new concepts.
- Keep large modules from growing further; add focused modules instead.
- Keep `match` statements exhaustive where possible.
- Collapse nested `if` statements when clippy would flag them.
- Inline `format!` arguments when possible.
- Prefer method references over redundant closures.
- Avoid ambiguous boolean or `Option` positional parameters in new APIs.
- Add doc comments to newly added traits.
- Do not use `#[async_trait]` or `#[allow(async_fn_in_trait)]` for new traits.

Never add or modify code related to
`CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR` or `CODEX_SANDBOX_ENV_VAR` unless the
official upstream change being merged already did so.

## App Server API Rules

All new app-server API work should target v2.

For v2 payloads:

- Use `*Params`, `*Response`, and `*Notification` names.
- Use `<resource>/<method>` RPC names with singular resources.
- Use camelCase on the wire unless the existing API explicitly requires
  otherwise.
- Export TypeScript types to `v2/`.
- Use cursor pagination for new list methods.
- Update `app-server/README.md` when API behavior changes.
- Regenerate schema fixtures with `just write-app-server-schema`.

Do not add new v1 API surface.

## Documentation Policy

Update documentation whenever a secondary-development change affects:

- user-visible behavior,
- configuration,
- commands,
- app-server API behavior,
- authentication,
- sandboxing,
- installation,
- developer workflow.

Private-only behavior should be documented in private secondary-development
docs, not hidden in code comments.

## Testing Policy

After Rust changes, run formatting from `codex-rs/`:

```bash
just fmt
```

Run the smallest relevant test first:

```bash
cargo test -p <changed-crate>
```

For TUI changes, update and review `insta` snapshots:

```bash
cargo test -p codex-tui
cargo insta pending-snapshots -p codex-tui
cargo insta accept -p codex-tui
```

Before finalizing a substantial Rust change, run scoped clippy fixes:

```bash
just fix -p <changed-crate>
```

If changes touch shared crates such as `common`, `core`, or `protocol`, run the
complete test suite after the project-specific tests pass. Ask before running a
full workspace test locally if it is expected to be slow.

Do not rerun tests only because `fmt` or `fix` was run after tests already
passed, unless the fix made a behavioral change.

## Dependency Policy

Avoid adding dependencies unless the benefit is clear and local alternatives are
not sufficient.

If Rust dependencies change:

```bash
just bazel-lock-update
just bazel-lock-check
```

Include `MODULE.bazel.lock` changes in the same commit as the dependency
change.

If adding compile-time file reads such as `include_str!`, `include_bytes!`, or
`sqlx::migrate!`, update the relevant `BUILD.bazel` metadata so Bazel builds
match Cargo builds.

## Conflict Resolution Policy

When syncing upstream:

1. Resolve conflicts by preserving upstream behavior unless the private change
   intentionally overrides it.
2. Keep conflict-resolution commits small.
3. Do not silently drop private behavior.
4. Do not silently fork upstream behavior in broad shared modules.
5. Add or update tests for any conflict whose resolution changes behavior.

If a conflict appears repeatedly, convert the private change into a clearer
extension point, feature flag, or isolated module.

Repeated conflicts are a design signal, not just a Git problem.

## Release Policy

Tag private releases separately from official upstream tags:

```bash
git tag private/v<version>
git push origin private/v<version>
```

Do not reuse official tag names for private builds.

Recommended version metadata:

- upstream base commit,
- private branch name,
- private release tag,
- build timestamp,
- enabled private features.

## Commit Message Policy

Use concise commit subjects:

- `feat: add private auth provider`
- `fix: preserve upstream config defaults`
- `docs: add secondary development guide`
- `sync: merge upstream main into codex/base`

Use `sync:` only for commits whose purpose is upstream synchronization.

## Pull Request Policy

Open pull requests against `codex/secondary-dev` or `codex/base`, not directly
against `main`.

Each PR should include:

- summary of private behavior changed,
- upstream files touched,
- tests run,
- known sync risks,
- whether docs or schemas were updated.

For UI changes, include snapshot updates in the same PR.

## Local Workflow Checklist

Before starting work:

```bash
git fetch upstream
git checkout main
git merge --ff-only upstream/main
git checkout codex/base
git merge main
git checkout -b codex/feature/<name>
```

Before opening a PR:

```bash
git status --short
just fmt
cargo test -p <changed-crate>
just fix -p <changed-crate>
```

Before merging a PR:

- Confirm the branch is based on the latest `codex/base`.
- Confirm tests and required generated files are current.
- Confirm no unrelated upstream sync churn is included.
- Confirm private behavior is documented.

## What Not To Do

Do not:

- Put private changes on `main`.
- Rewrite shared branch history casually.
- Mix upstream sync and feature work in one commit.
- Add broad changes to `codex-core` by default.
- Add app-server v1 API surface.
- Accept snapshot changes without reviewing them.
- Hide private behavior only in comments.
- Reuse official release tags for private builds.

## Maintenance Cadence

Recommended cadence:

- Daily or before each work session: fetch upstream.
- Weekly: merge `upstream/main` into `main` and `codex/base`.
- Before large feature work: create a fresh branch from `codex/base`.
- Before private release: sync upstream, resolve conflicts, run required tests,
  and tag with a `private/` tag.

The longer the fork goes without syncing, the more expensive the next sync will
be. Prefer small, frequent upstream merges.
