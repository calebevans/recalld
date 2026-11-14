# Legacy structured-label admission

The legacy writer stores entities, topics and emotions as normalized prefixed tags. Previously an accepted label such as `John Smith`, `José` or `Alice's` failed tag encoding and disappeared before the canonical insertion hash. A subsequent derived rebuild lost its temporary entity posting.

Supplied raw tags also reject empty or invalid format entries with their original index instead of silently dropping them. Tag normalization remains lowercase.

HTTP single/batch store, MCP parsing and the concrete MCP/daemon storage adapter now reject unrepresentable structured labels before embedding-provider calls, UUID assignment or payload allocation. Errors identify the field and item index; HTTP batch reports the failed item without committing it. Empty labels also fail. Existing ASCII tag-compatible labels retain legacy lowercase normalization.

This corrects the public legacy input contract. Clients must send nonempty labels that fit the existing prefixed-tag format: ASCII letters/digits and `/ _ . : -`, within existing byte/count limits. The API does not invent a lossy replacement or claim durable Unicode support. Typed native writers with canonical structured-label persistence are the prerequisite for broader acceptance.

Generic model/schema validation and reads of already-retained canonical UTF-8 labels are unchanged. No warm-only index preservation, new provenance, native serving activation or batch atomicity is introduced.

Validation on the minimal published insertion parent: 647 library tests and 1 CLI integration test passed; 4 doctests ignored. Formatting passed; Clippy exited 0 with warnings (371 library, 382 test including duplicates). The initial tests cover 15 rejected field/label combinations through real HTTP single/batch handlers, concrete MCP storage and MCP parsing, asserting field/index errors, provider count zero, unchanged generation and byte-identical fixture files. A positive normalized-label case survives two real insertions, derived rebuild and reopen. All ran offline in recalld-ci:1.94/Rust1.94.1, 2 CPUs/4GiB/2 Cargo jobs, with no home, credentials or sockets mounted. Raw output: /tmp/recalld-lifecycle-review/legacy-label-full.log.

Raw-tag follow-up: the same full offline suite again passed 647 library + 1 CLI tests, 4 doctests ignored, fmt and Clippy (warnings retained). The unchanged-byte/provider-zero matrix now covers 20 field/label combinations, including `tags`, and the positive case verifies `USER/Chosen` is retained as `user/chosen` after restart. Final output: /tmp/recalld-lifecycle-review/legacy-label-tags-full.log.
