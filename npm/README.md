# triad-harness

This package provides the `triad` CLI through npm/npx on macOS:

```bash
npx triad-harness --help
```

The launcher downloads the matching binary from the package version's GitHub Release and verifies it against the published SHA-256 checksum before execution. It supports Apple Silicon and Intel Macs.

Triad uses existing subscription logins from Claude Code, Codex, Kimi Code, and Cursor Agent. This npm package does not contain provider credentials or API keys.
