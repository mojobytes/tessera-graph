# TDD Plan: LBAC Quality Fixes (#1, #2, #3, #10, #11, #12)

**Created**: 2026-03-23
**Status**: Pending

## Items

1. Explicit write-dominance check on update_node/update_edge
2. Replace expect() panic with let-else in resolve_clearance_or_deny
3. add_node/add_edge inherit caller's clearance level (Bell-LaPadula no write-down)
10. Doc comment on pub mod filter
11. SecureGraph read coverage gaps (edges_by_label, incoming_edges)
12. Revoked session integration test
