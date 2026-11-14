# 2026-11-14: batch admission correction

Minimal runtime `98dbef6`, parents `e51cb67` and actual PR68 `b2923d1`.
The review found that the reserved batch adapter bypassed strict legacy label
admission and could drop labels before canonical hashing. Shared validation now
precedes canonical conversion/provider/staging at the real entry. Original
caller hashing stays unchanged so historical committed receipts replay; old
pending invalid input finalizes a bounded indexed rejection.

Validation: 3 new plus 2 inherited focused tests; 722 library and 1 CLI passed;
four doctests ignored; fmt passed; Clippy exited 0 with warnings. Actual MCP and
framed daemon each exercised 14 invalid cases, allowing only the documented
parent reservation generation, with no child record/payload/provider effects.
Positive canonical reopen and historical receipt/pending controls passed.

[Evidence, exact source/binary hashes and raw logs](../evidence/batch-legacy-admission/README.md).
One initial diagnostic-case assertion failed and was corrected; it was not a
production failure. No source-root switch, power-loss, native-v2, whole-store CAS
or complete storage-contract claim. Main owns publication. The accepted-work
join consumes this runtime separately; offline compatibility remains Feynman's
independent consumer.
