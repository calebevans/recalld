# Offline canonical snapshot

`storage::offline_snapshot::OfflineSnapshot::open(path)` acquires stable ownership and the legacy directory lock, validates the canonical inventory, and reads metadata/edges from disposable database copies. Payload handles and ownership remain held until drop. It does not construct a serving engine, initialize canonical state, replay effects, contact providers or enable schema-v2 writers.

`control()` and `namespaces()` expose the pinned generation and canonical policy/profile configuration. `record(id)` and `for_each_record` return decoded `DiskRecord`, retained text and optional vector values. `for_each_edge` returns validated edges. Missing records return `None`; tombstones have no embedding. Callbacks are bounded per record; the existing complete inventory validator retains its whole-store ownership/endpoint sets.

This is the sanctioned offline read boundary for logical export and tests of the actual query engine against a pinned synthetic canonical store. It grants no normal daemon registration or readiness. Integrated validation must reject pending canonical effects and malformed ledger/ancestry state, rather than applying effects during inspection. Unknown required formats remain errors.

The offline regression checks real schema-v2 reads, pinned generation/namespaces, exclusive ownership, continued ordinary-open rejection and unchanged canonical bytes. It runs in the resource-limited offline Rust 1.94.1 Docker environment.
