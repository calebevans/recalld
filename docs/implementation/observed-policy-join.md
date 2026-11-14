# Minimal shipping update for Observed consumers

Measured source `424ea7a3baf94c765f03ed751eef78dc0c029203` on existing branch `impl/observed-consumer-join` (#58). This adds only the approved actual prerequisite stacks to prior58 `3b590fb28c84bb2aa2d8afaac66e34556d7da6a6`:

- Finalized#57 `129b701e590cfef39ba51cc6ac467725510e4cd9` (65edaa3 code plus reproduction evidence), merged as087f6a6.
- Current#40 `e7fd45573b7ba2de4fd216a208f7981a0a8c8c2d`, merged asb08331d, including its actual ownership-test prerequisite.
- Hume62dbd44 real schema/lifecycle migration fixture, cherry-picked as41b2b66.
- Only the by-ID timestamp overlay from3deecca, applied as424ea7a in `src/search/strength_tests.rs`.

No catalog/export/evaluation/broad-integration or entity-rebuild parent. `ancestry.json` verifies positive parents and negative ancestry for75882c3,098a182,c425a5d,7fb2769,e64930d,faf0f81,159f987. #57's actual restore/provenance ancestors are required and retained; no broad runtime tree was copied.

## Production and fixture seams

The two merge conflicts were in `src/storage/control.rs`. Both observation outcomes and LifecyclePolicyMigrated remain, with all their validators. Lifecycle receipts require canonical schema2. Observation receipt key scope remains checked. Restored receipts use57's permitted-origin ancestry check; the obsolete unconditional comparison with the current generation was removed. The generic outcome arm accepts LifecyclePolicyMigrated only alongside its owning canonical-schema validation. Original requested observation mode, one clock, canonical hydration, returned-only exposure and receipt-carrying final admission are unchanged.

Strength fixtures perform actual legacy insertion, schema migration, then receipt-backed lifecycle migration. The by-ID test's independent retrieval timestamps are an explicitly synthetic in-memory CachedRecord overlay over that valid snapshot. Original projection/donor/scope/clock-count/byte assertions remain, and canonical unknown retrieved time is asserted. This does not fabricate a persisted native exposure. The fixture adaptation now ships with58 rather than remaining only on a broad validation branch.

## Exact isolated proof

- 775 library tests passed,0failed, normal parallel execution,156.42s.
- 2 CLI integration tests passed (lifecycle1,text migration1). No offline_eval target exists on this minimal stack; no example or broad CLI count is claimed.
- 7 Python runner checks,formatting,Clippy anddebugserver build passed. Clippywarnings remain (412lib,435lib-test including412duplicates,1bin).
- 9/9 public episodes passed:3freshstdioMCP,3HTTPMCP,3REST;54returned-only true cases including18empty no-write cases.
- All9 false-mode public/durable-byte comparisons,wrong-quality rejection and restart checks passed.
- 18 actual postcommit fault cases passed overHTTP/MCP/frameddaemon get/search/similar. Errors retain persisted committed receipts, exactlyone event perexposedID and no selected payload.
- 12 precommit hydration-error controls,2finaltransaction guards and1corruptlegacy-policy startup guard passed. Strict public/hash checker passed.

Same unchanged harness3642034/fixture3, image `sha256:daefdc9cbe9b0f39b1fc29d975148b3b10adc1bd4309f9ca1be8abd36ed811ef`,Rust1.94.1,Python3.11.2,aarch64,networknone,2CPU/4GiB,2buildjobs,testlink-j1,private copiedregistry/target. No home,credentials orsocketmounts.

```sh
python3 scripts/public-conformance.py --fixture-version 3 --source-root /tmp/recalld-implementation/observed-consumer-join --source-commit 424ea7a3baf94c765f03ed751eef78dc0c029203 --source-input tests/text_migration_cli.rs --cache /tmp/recalld-unit-tests-l__ja1yo/cargo --build-cache /tmp/recalld-public-conformance-7lgc6nhu/target
```

Outputs `/tmp/recalld-public-conformance-frkabglm/output`, rawfaults/exactcontainercommand in siblingtargeted. Verified170source+164output+4targeted hashes including archive contents. Archive `validated-424ea7a.tgz` SHA256 `f9ca29f64294c3bcdd043ba5bd07d55645f4ad71e5e25e92d05f6b0c9454b2d4`. Work-notes artifact destination `Experiments/Artifacts/recalld/2026-11-14-observed-minimal-424ea7a`, with COPY-MANIFEST. Evidence is appended to the existing study/observed-integration branch forPR64; original364,525 andc425 artifacts stay unchanged.

No push,newPR,CI orreleasegate claim. Actualwallclock and bounded false-byte observations remain; only fts.db-shm coordinationbytes are excluded from durable equality, with rawhashes retained. This is not a transitive barrier,frozenreset,nativepublic-v2 orfullA6 certification. Subsequent actual owned-core tracker/freeze implementation is coordinated with Curie/Feynman; unregistered producers,purge/privateguard bypasses anduntrackedderivedwatermarks remain explicit gaps until the proper core join.
