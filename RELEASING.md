# Releasing Triad

Triad publishes macOS binaries to GitHub Releases, an npm launcher for `npx`, and a Homebrew formula mirrored by `nocell/homebrew-tap`.

## One-time setup

1. Create the public npm package `triad-harness`. For the first publish, add a short-lived granular npm automation token as the GitHub Actions secret `NPM_TOKEN`.
2. After the package exists, configure npm trusted publishing for repository `nocell/triad-harness`, workflow `release.yml`, allowed action `npm publish`. The npm CLI equivalent is `npm trust github triad-harness --repo nocell/triad-harness --file release.yml --allow-publish -y`. Then delete `NPM_TOKEN`; subsequent releases use GitHub OIDC and receive npm provenance automatically.
3. Add a newly generated OpenRouter key as `OPENROUTER_API_KEY` only if the optional live E2E workflow should run. Never put the value in Git, workflow YAML, command arguments, issues, or pull requests.

## Release

Keep the versions in `Cargo.toml` and `npm/package.json` identical, commit the change, then push the matching tag:

```bash
git tag v0.1.0
git push public v0.1.0
```

The release workflow tests both native macOS architectures, uploads checksummed archives and `triad.rb`, and publishes `triad-harness` to npm. The Homebrew tap synchronizes the formula from the latest GitHub Release without a cross-repository token.

Do not reuse a tag after any release artifact has been published. Increment the version and create a new tag instead.
