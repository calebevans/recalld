# Canonical batch legacy label admission

This correction joins canonical batch head `e51cb67ad5442c99c4b7d9c585dfed91d7448ba1`
with the required shared legacy validator from PR68,
`b2923d1c0240b3a0f6406feefd956bd5a58c9f48`. No native writer, label cache,
accepted-work tracker or public contract2 dependency is added.

The shared reserved/single adapter validates raw tags and structured labels
before canonical conversion, replay normalization, provider calls or staging.
`canonical_input` now returns a validation error instead of allowing its lossy
legacy tag parser to erase supplied fields. It reuses PR68's validator; there
is no parallel label policy or temporary entity-cache workaround.

Batch identity still parses the original generic fields and expands the same
fixed defaults before the representation check. Invalid Unicode, space,
apostrophe and empty labels retain their original caller hash. Fresh invalid
items produce indexed `INVALID_INPUT` outcomes with the field name. An accepted
all-invalid batch advances generation for its catalog reservation; it does not
call a provider, stage record/vector/text data, or create successful child
receipts. Matching finalized failures replay; changed raw invalid input conflicts.

A pre-correction manifest can already exist. Matching historical committed
outcomes continue replaying their original receipt and response identity without
a provider or new canonical write. This does not recover labels previously
lost by the old writer. A historical pending item is revalidated and cannot
bypass the shared validator; its terminal rejection has the existing separate
catalog bookkeeping receipt. Parent replay still precedes mutable policy checks.

Single/unreserved writes retain PR68's early rejection with no mutation. Broader
Unicode support requires a canonical native representation; this correction
keeps the legacy ASCII prefixed-tag contract.

Runtime `98dbef6` passed 3 new + 2 inherited focused tests, 722 library + 1 CLI tests, format and Clippy (warnings retained); four doctests were ignored. Exact source/binary hashes are recorded in
[the evidence package](../evidence/batch-legacy-admission/README.md). All fixtures
are synthetic temporary stores. Process reopen is not a power-loss experiment.
