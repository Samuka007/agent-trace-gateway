//! Harness layer: WHO fills the protocol layer's mount points, and in what
//! shape. Phase-2 target: HarnessDescriptor tables (claude-code / codex /
//! grok / opencode) + a strength-ordered identification engine. Dependency
//! red line: atg-harness → {atg-protocol (mounts), atg-model} — never the
//! gateway/trace layer.
//!
//! Phase 1 placeholder: the workspace skeleton enforces the dependency
//! direction already; the descriptors land in the harness phase.
