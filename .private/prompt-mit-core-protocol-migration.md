# Prompt for MIT Core Session — Migrate Bolt Protocol Crate

## Task

Move the Bolt 4.4 protocol implementation from the enterprise repo into the
MIT core as a new crate `tessera-graph-protocol`. This enables the future
`tessera-graph-client` WASM crate (independent repo, MIT license) to speak
Bolt without depending on the proprietary enterprise crate.

## Source

Enterprise repo: `tessera-graph-enterprise/crates/tessera-graph-protocol/`

The code has already been cleaned — dead legacy modules (`frame.rs`,
`message.rs`) were removed in commit `36965a2`. What remains is pure
Bolt 4.4 protocol implementation:

```
src/
├── bolt_client.rs      (279 lines) — high-level Bolt client: connect, hello, run_query
├── bolt_frame.rs       (179 lines) — Bolt chunked framing (spec-compliant)
├── bolt_handshake.rs   (79 lines)  — Bolt version negotiation
├── bolt_message.rs     (343 lines) — HELLO, RUN, PULL, DISCARD, SUCCESS, FAILURE, RECORD
├── error.rs            (55 lines)  — ProtocolError enum
├── packstream/
│   ├── decoder.rs      (348 lines) — PackStream binary deserialization
│   ├── encoder.rs      (239 lines) — PackStream binary serialization
│   ├── markers.rs      (34 lines)  — PackStream type marker constants
│   ├── mod.rs          (12 lines)
│   └── value.rs        (85 lines)  — PackStreamValue enum
├── tls.rs              (145 lines) — TLS config builder (rustls wrapper)
└── lib.rs              (27 lines)  — public API re-exports
```

Total: ~1,825 lines. No proprietary logic — all based on the public
Bolt 4.4 protocol specification.

## Dependencies

```toml
[dependencies]
thiserror       = "2"
rustls          = "0.23"
rustls-pemfile  = "2"
tokio           = { version = "1", features = ["io-util"] }
bytes           = "1"

[dev-dependencies]
rcgen    = "0.13"
tempfile = "3"
tokio    = { version = "1", features = ["rt", "macros", "io-util"] }
```

## Steps

1. Create `tessera-graph/crates/tessera-graph-protocol/` as a new workspace member
2. Copy all `src/` files from the enterprise protocol crate
3. Copy test files from `tests/` (bolt_handshake_test.rs, bolt_message_test.rs,
   packstream_test.rs, tls_test.rs — check what exists)
4. Update the copyright header from "BelowZero Security OU" to MIT license
5. Add the crate to the workspace `Cargo.toml`
6. Verify: `cargo test -p tessera-graph-protocol` passes
7. Verify: `cargo clippy -p tessera-graph-protocol -- -D warnings` clean

## After migration

Once this is in the MIT core, the enterprise repo will:
1. Remove its `tessera-graph-protocol` crate
2. Add a path dependency to the MIT core version:
   `tessera-graph-protocol = { path = "../tessera-graph/crates/tessera-graph-protocol" }`
3. All existing code (`bolt_handler.rs`, `tessera-graph-cli`, benchmark) continues
   working with no changes — same API, different source location

## What NOT to do

- Do NOT modify the API — it must remain identical for the enterprise swap
- Do NOT add features or refactor — just move and re-license
- Do NOT touch the GQL code — that's a separate task (Block 1-3)
