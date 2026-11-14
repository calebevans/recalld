# 2026-11-14 namespace purge writer

Approved purge slice implemented on published tombstone `dad8465` and current
catalog `d4f756c`, runtime `f6e4d9263fe2d96fca16282eed47b1251ad5398b`.
Lazy versioned progress, immutable start/child/final receipts, reserved child IDs,
canonical chunk/ledger/effects commits, and a name lease through directory fsync
replace an untracked partial-deletion loop. Startup and the real online
completion coordinator resume accepted work; publication failure remains blocked
until completion retry or storage-uncertainty reopen.

Independent review caught a real error-mapping issue: the wrapper could put a
previous receipt outside the failing child's typed committed/unknown error.
The fix preserves the current typed error. Actual MCP fault tests assert the
exact child receipt/unknown operation ID and reconcile without repeating removal.

Final isolated offline validation: 698 library + 1 CLI passed, zero failed;
4 doctests ignored; fmt passed; clippy completed with 411 library / 431 test
warnings including duplicates. Six process-exit boundaries, malformed progress
preflight byte preservation, name reuse, online failure/retry, and actual backup
and schema migration history preservation are covered. No power-loss claim.
[Evidence](../evidence/namespace-purge/README.md).

Published export49 `faf0f81` and the newer lineage-changing restore explicitly
reject these canonical tables for now. Feynman owns the follow-up: capture and
merge target completed progress with target receipts, preserve origin lineage
and IDs, validate ancestry, then enable lossless export/import. Allowlisting alone
would lose target progress and create orphan receipts. Active/Cleanup remains
blocked offline. Whole-meta snapshots/migrations preserve completed rows.

Feynman also owns the maintenance/raw-writer guard join. Halley owns accepted
work/freeze/barrier, Pascal native writers. Our next durable batch manifest and
actual consumer slice remains separate. This purge slice is not whole-store
CAS, a leased query read view, native-v2 serving or full A1 completion. Main owns
publication; no push/deployment by this agent and no personal recalld access.
