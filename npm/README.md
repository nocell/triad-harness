# triad-harness

This package provides the `triad` CLI through npm/npx on macOS and Linux:

```bash
npx triad-harness --help
```

The launcher downloads the matching binary from the package version's GitHub Release and verifies it against the published SHA-256 checksum before execution. It supports x86_64 and ARM64 on macOS and Linux.

Triad supports up to six parallel reviewers across five vendor subscriptions: Claude Code, Codex, Kimi Code, Cursor Agent, and native ZCode. ZCode provides separate `zcode` (`GLM-5.3`) and `zcode_flash` (`GLM-5.3-Flash`) reviewers through Z.ai Coding Plan login; both stay parallel in Default, Easy, and Ultra presets. This npm package does not contain provider credentials or API keys.

The official ZCode bundled CLI discovery candidate on macOS is `/Applications/ZCode.app/Contents/Resources/glm/zcode.cjs`. Triad does not install ZCode or start login automatically. Discovery and fake-CLI tests do not prove live authentication or inference. `GLM-5.3-Flash` is the official model name; Instant is not a separately verified model, and FlashX is not a substitute.
