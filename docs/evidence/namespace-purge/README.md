# Namespace purge implementation evidence

Exact tested runtime: `f6e4d9263fe2d96fca16282eed47b1251ad5398b`.
698 library and 1 CLI tests passed, zero failed; 4 doctests ignored. Formatting
passed; clippy completed with warnings. The [manifest](manifest.json) records
source hashes, commands, image and retained/full-log hashes.

[Selected full-suite output](tests-summary.log) includes every purge proof and
the suite totals. [Current-child transport proof](child-identity.log) retains the
focused regression result. [Clippy summary](clippy-summary.log) records successful
completion without pretending warnings were absent.

[Parent join failures](parent-join-failures.log) records the initial 684 pass /
2 fail join: diagnostic-string mismatch and an old insertion fixture expecting
schema-bearing catalog creation. `97f0fd9` preserved both production checks and
fixed the test boundary; all three focused cases then passed. The first purge
run also exposed an incorrect legacy `directory_removed=true` result when its
directory was already absent; the corrected implementation preserves false.
These development failures are not counted as final passes.

All fixtures are synthetic temporary stores. The process-child tests exit at
start commit, record commit, edge publication, canonical acknowledgement and
owned directory cleanup boundaries, then reopen twice. No power-loss guarantee
is inferred. No personal store/daemon/provider access, deployment or publication
was performed. Completed-history logical export and newer production restore
remain explicitly unsupported pending the separate ancestry/target-merge child.
