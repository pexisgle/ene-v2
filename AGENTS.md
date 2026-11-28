Read README.md and the relevant specifications under docs/ before changing behavior.

- Keep the workspace dependency graph acyclic. Share the generated RPC types directly at the host and desktop boundary. Keep pure approval and prompt rules in ene-core.
- Keep host state transitions on the single actor. Perform model, plugin, and external IO in cancellable jobs with identity-checked completions.
- Use anyhow for internal errors. Translate failures into Connect statuses at the boundary and keep credentials and private context out of diagnostics.
- Never publish a tool call before its approval is bound to the exact receiver, normalized arguments and applicable run or device. Test changes to these invariants.
- Preserve the ene-vrm implementation. Keep platform-specific window and input code at the edge of the desktop and avatar apps.
- Run cargo fmt --all -- --check, cargo clippy --workspace and cargo test --workspace.
