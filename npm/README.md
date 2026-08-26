# triad-harness

This package provides the `triad` CLI through npm/npx on macOS and Linux:

```bash
npx triad-harness --help
```

The launcher downloads the matching binary from the package version's GitHub Release and verifies it against the published SHA-256 checksum before execution. It supports x86_64 and ARM64 on macOS and Linux.

Triad uses existing subscription logins from Claude Code, Codex, Kimi Code, and Cursor Agent. This npm package does not contain provider credentials or API keys.
