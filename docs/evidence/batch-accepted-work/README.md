# Batch accepted-work validation

Runtime **12e53e0** consumes actual published accepted-work `05cd254`, batch
`e51cb67` and the independent validated batch/PR68 correction `9d8b295`.
The preceding tracker merge is `8f9b8d1`; no native writer, offline compatibility
or whole-core control branch was added. [manifest.json](manifest.json) binds exact
parents, tested source files, executable hashes, commands and raw log hashes.
[Changed source files](changed-files-vs-corrected-batch.txt) and the compressed
runtime patch compare against the corrected batch parent.

Final: **43 batch + 14 tracker/metadata + 9 coordinator focused checks;
744 library + 1 CLI tests passed**, zero failed, four doctests ignored. Format
passed; Clippy exited 0 with 481 library / 499 test warnings (477 duplicates),
plus one binary warning. Full and focused raw logs are retained compressed.

Docker image ID is pinned in the manifest: Rust1.94.1, network none, two CPUs,
4 GiB and two Cargo jobs, private cargo/target, no home/credentials/socket mounts.
The copied private target was package-cleaned before compiling this source root.
There was no source-root switch. Final full validation mounted source read-only
and verified its hashes before/after; the commit was created afterward from those
same bytes. This is functional evidence from synthetic temporary stores, not a
performance measurement or power-loss experiment.

The new tests cover admission closed before reservation/provider, accepted
provider and actual optional graph publication finishing during closing, final
runtime error bookkeeping during closing, accepted reserved tombstone children,
cancellation/reopen, unknown current-child identities, provider failure with
stopped execution but durable pending payload, completed all-invalid manifests,
cooperative checks between lifetime rows, and actual framed daemon rejection.
The inherited PR68 and new batch label tests also run in the combined suite.

Integration adjustments beyond the parent merges:

- Thread the accepted root through reservation, replay lookup, insertion/forget,
  runtime rejection bookkeeping, derived completion, and optional graph/cache/ack
  descendants. Register queued mutation workers before enqueue.
- Keep the same metadata mutation guard through canonical effects and
  acknowledgement; retain `Option<&BatchChild>` as the final effect-tail argument.
- Check canonical pending batch items during metadata capture, with cooperative
  deadline callbacks between manifest rows and unchanged strict startup proof.
- Keep the strict shared legacy validator inside the work-aware reserved/single
  entry before canonical conversion/provider calls; preserve caller hashing and
  historical replay from the independent admission correction.

Two developmental test failures are retained with source and binary hashes. The
first required a cache hit even though a canonical-bound safe miss is valid. The
second expected incident counts instead of the canonical outgoing count. These
were test corrections, not production failure reproductions. One early formatter
attempt saw an incomplete bind-mounted file immediately after a host edit;
subsequent final validation used settled read-only source and explicit hashes.
Inherited accepted-work patch evidence is preserved byte-for-byte, including its
patch-context whitespace; source/docs whitespace checks passed.

Remaining scope: all-writer/whole-core readiness and generation-targeted barriers,
private snapshot/readview integration, native participation, controlled clock/RNG
and cache state, and the independent frozen offline consumer `3b0aded` at its actual join. The lifetime
manifest scan stays linear; callbacks cannot preempt redb, decode or OS I/O.
No hard deadline, public whole-store CAS, contract2 activation or full A1 claim.
