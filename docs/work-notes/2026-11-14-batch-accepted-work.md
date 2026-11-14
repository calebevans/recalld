# 2026-11-14: batch accepted-work integration

Runtime `12e53e0`, initial tracker merge `8f9b8d1`, actual accepted-work parent
`05cd254`, corrected batch parent `9d8b295` (batch `e51cb67` plus PR68 `b2923d1`).
The actual batch root covers reservation/provider/children and optional graph
publication. Child workers remain admitted during closing; cancellation and
unknown work remain unresolved. Stopped execution cannot conceal a durable
payload-required batch slot during metadata capture.

**43 batch + 14 tracker/metadata + 9 coordinator focused; 744 library + 1 CLI
passed**, four doctests ignored, fmt passed, Clippy exited 0 with warnings.
The full run used read-only hash-checked sources in offline Docker, Rust1.94.1,
2CPU/4GiB, private package-cleaned cache/target. No source-root switching.

[Evidence, exact source and binary hashes, raw logs and source adjustments](../evidence/batch-accepted-work/README.md).
Two test-development failures incorrectly required a cache hit and an incident
edge count; both are retained without claiming a production red/green experiment.
Process/reopen tests do not demonstrate power-loss durability. The pending scan
checks the deadline between lifetime manifests, with no hard OS bound.

No full storage contract, native activation, public CAS, contract2, whole-core
readiness or frozen clock/cache/RNG claim. Halley owns remaining whole-core
control integration; Feynman's independent offline consumer is frozen `3b0aded`;
Pascal's native effect tail still receives the real guard and optional child.
Main owns publication; no push or deployment was performed.
