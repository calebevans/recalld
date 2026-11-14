# Mutation admission gate

This independent slice depends on mutation foundation `dfe2078`. Source commit
`008ea074dc47ec4e5e75ba5a1294939c16c946da` adds a sticky, process-local unavailable
flag shared by the engine and its metadata store. Compaction and canonical
edge effects may depend on this gate independently.

Ordinary engine operations and public metadata reads reject an unavailable
handle with `StorageError::RecoveryRequired`. Metadata writers check again
**after acquiring** the publication write guard, so a queued writer cannot
proceed after the preceding writer has invalidated the handle. Metadata commit
errors mark it unavailable; receipt commits preserve the operation ID in
`CommitOutcomeUnknown`. Callers of `mark_unavailable` must hold the storage
write guard or the metadata publication guard. For graph writers the shared
order is graph owned write, storage write, metadata publication, then redb.
No storage method acquires an asynchronous graph lock.

Admission is an atomic-bool check at each engine/metadata entry, not a lease
held across the whole query pipeline. Cached `QueryEngine::get_memory` and
`similar` paths, and direct graph readers, can currently bypass canonical
admission. This slice does **not** make all public reads unavailable. Effects
must add consumer admission coherence; a future canonical read view remains
necessary to cover every writer and derived read.

The flag is never reset on a live handle. Reopen must perform the relevant
recovery protocol before admitting operations. This gate does not itself add
an on-disk recovery marker, cancel in-flight work, implement a pinned canonical
read view, gate every out-of-storage cache/graph API, or certify whole-store
CAS/readiness. Raw backend accessors remain internal integration seams.
Compaction/effects must persist recovery evidence and invalidate on uncertain
commit or failed publication. Newly introduced methods on other branches,
including `records_in_namespace`, require admission checks during integration;
they are not covered by this branch's validation.

## Validation

Executed 2026-11-14 at source `008ea07`, parent `dfe2078`, in isolated Docker
`recalld-ci:1.94`, Rust `1.94.1`, `--network none --cpus 2 --memory 4g`, with
`CARGO_BUILD_JOBS=2`. Only this worktree and its private Cargo/target scratch
directories were mounted. No host home, credentials, sockets or personal data.

| Check | Result |
| --- | --- |
| `cargo fmt --all -- --check` | Passed |
| `cargo test --offline --locked --all-features -- --test-threads=2` | 531 passed; 4 existing doctests ignored |
| `cargo clippy --offline --locked --all-targets --all-features` | Passed with warnings; not warning-free |

The new temporary-store regression poisons a handle and checks metadata,
text/vector/edge reads, delete, direct metadata mutation and sync rejection;
a real reopen preserves the original record bytes. Existing publication and
receipt tests also passed. This is not an I/O fault-injection or crash-recovery
claim. Compaction/effects supply their own durable-boundary crash tests.
Logs and caches are untracked under `.validation/` and excluded from commits.
