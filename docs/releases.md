# Releasing recherche

Release-please maintains the release PR, version and changelog. Merge feature
and fix PRs into `main`, review the release PR, then merge it. The Release
workflow creates the tag and GitHub Release, builds platform archives, and
publishes the Homebrew formula automatically.

Merge the publishing workflow before the first release PR (#9).

## One-time setup

Set the `JRELEASER_HOMEBREW_GITHUB_TOKEN` GitHub Actions secret in the recherche
repository. Use a fine-grained token with **Contents: Read and write** access
to `manojkarthick/homebrew-tap`. The built-in `GITHUB_TOKEN` publishes assets
in recherche but cannot write to a separate repository. A secret configured
in another repository is not inherited here.

Do not put the token in configuration files or command arguments. When loading
it locally, use the project's 1Password Environment workflow and pass it to
`gh secret set JRELEASER_HOMEBREW_GITHUB_TOKEN` on standard input.

## What gets published

JReleaser 1.27.0 packages `bin/recherche` and the MIT license in ZIP archives
named `recherche-<version>-<target>.zip`, and publishes SHA-256 checksums:

| Target | Runtime baseline |
| --- | --- |
| `aarch64-apple-darwin` | Apple Silicon, macOS 15 or newer |
| `x86_64-unknown-linux-gnu` | Intel/AMD 64-bit, Ubuntu 24.04 or compatible newer system |
| `aarch64-unknown-linux-gnu` | ARM64, Ubuntu 24.04 or compatible newer system |

Linux binaries use system glibc, libstdc++, OpenSSL 3 and CA certificates.
On Ubuntu these are supplied by `libc6`, `libstdc++6`, `libssl3t64` and
`ca-certificates`. macOS binaries use system frameworks. Model weights download
on first use; they are not included in the archives. Intel macOS is deferred
because the pinned ONNX Runtime crate has no prebuilt library for that target.

Each native build extracts its archive into a temporary directory, checks
version/help and linked libraries, and runs embedding and reranking against a
synthetic session with isolated data and configuration. Model downloads require
network access. A model failure fails the release build even when the CLI can
fall back to keyword search.

After all builds pass, JReleaser updates only the assets of the existing
GitHub Release. It preserves release-please's notes and tag. The generated
formula is installed and tested on each platform before JReleaser commits
`Formula/recherche.rb` directly to `manojkarthick/homebrew-tap`.

## Retry a failed release

Open Actions → Release → Run workflow on `main` and enter the existing tag,
for example `v0.1.0`. Only stable `vX.Y.Z` tags with an existing GitHub Release
are accepted. All build jobs check out that tag and verify Cargo's version.

This rebuilds and retests all archives, replaces matching release assets and
checksums, and republishes the formula after its installation tests pass.
If the tap token was missing, configure it before retrying. A failure before
the tap update leaves the previous formula in place; uploaded release assets
may already be available. Workflow concurrency serializes releases and retries.

Watch the Release workflow for failures. Do not merge a subsequent release PR
until the previous release's publishing run succeeds. Rebuilding older tags
after a newer release would move the formula back to the older version.

No signing, notarization, crates.io publishing or Homebrew service definition
is configured.
