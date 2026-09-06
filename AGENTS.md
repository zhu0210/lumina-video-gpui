# Agent Rules

## Scope and workflow

Read `CONTEXT.md` and relevant decisions in `docs/adr/` before changing architecture.
Preserve existing user changes. Prefer small fixes to shared causes over parallel implementations.
Use `rtk` for shell commands when available; `rtk proxy` supports commands without a dedicated wrapper.
Local instructions guide implementation; they do not require extra approval for work the user already authorized.

## Validation

Before committing Rust changes, run `cargo fmt --check`, `cargo check`,
`cargo clippy -- -D warnings`, and `cargo test` for the affected workspace/packages.
Run `cargo fmt` when formatting needs updating. Documentation-only changes need no Rust build.
Run feature and target checks only for features present in the manifests and targets/toolchains
available on the host. Report unavailable checks and pre-existing failures accurately; do not
change unrelated code or weaken tests just to make checks pass.

Portable manifests must not contain absolute local paths. Relative path dependencies must
resolve inside the workspace. Use pinned git dependencies for maintained external projects;
temporary local Cargo overrides are allowed for cross-repository validation but must not be committed.
Check manifests with `rg 'path\s*=\s*"/' --glob Cargo.toml` (no matches is success).

## Correctness and media performance

- Handle fallible input without panicking. Avoid `unwrap()`/`expect()` in production library code;
  tests may assert invariants. Index only where bounds are proven; otherwise use checked access.
- Keep blocking I/O, waits, avoidable allocations, and contended locks out of render/per-frame paths.
- Keep queues bounded. For live overload, drop stale frames rather than accumulate latency.
- Keep one presentation authority per media session; preserve audio/video drift correction.
- Seek and network discontinuities must have bounded resynchronization and a timeout.
- Use native-memory frame delivery as the normal playback path. CPU pixel upload is only a last
  fallback after native import is unavailable or fails; expose the reason and never label it zero-copy.
- Every unsafe block/implementation needs a `SAFETY` comment explaining ownership, lifetime,
  synchronization, and other relevant invariants.
- Prefer state owned by structs. Process-wide runtime initialization is allowed when required by
  platform APIs; avoid mutable global playback state.
- Use dependencies rather than copying external implementations into this repository.

## Project references

Cargo manifests and target-specific implementations define current platform/feature support.
Do not assume legacy feature names or backend descriptions still apply.
See `docs/MOQ_BEST_PRACTICES.md` when changing MoQ transport,
`docs/agents/issue-tracker.md` for tracker operations,
`docs/agents/triage-labels.md` for labels, and `docs/agents/domain.md` for domain documentation.
