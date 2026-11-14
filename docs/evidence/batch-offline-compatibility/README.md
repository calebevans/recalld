# Batch offline compatibility evidence

See [implementation report](../../implementation/batch-offline-compatibility.md).
`manifest.json` records exact source files, log digests and five executables selected by the real full-run log. `.log.gz` files contain the original unmodified logs. No build output or cache is committed. Runtime composition ce66f75; later sealed PR72 parent merge a88631b changes documentation/evidence only. Standalone proof does not certify the PR70 compatibility join or native serving.
