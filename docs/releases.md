# Releasing recherche

Release-please maintains the release PR, version and changelog. Merge feature
and fix PRs into `main`, review the release PR, then merge it. The Release
workflow creates the tag and GitHub Release, builds platform archives, and
publishes the Homebrew formula to `manojkarthick/homebrew-tap`.

If a publishing job fails, use **Re-run failed jobs** on that workflow run.

## One-time setup

Set the `JRELEASER_HOMEBREW_GITHUB_TOKEN` GitHub Actions secret in the recherche
repository. Use a fine-grained token with **Contents: Read and write** access
to `manojkarthick/homebrew-tap`. The built-in `GITHUB_TOKEN` publishes assets
in recherche but cannot write to a separate repository.

## What gets published

JReleaser packages `bin/recherche` and the MIT license in ZIP archives named
`recherche-<version>-<target>.zip`, with SHA-256 checksums. It only adds
assets to the GitHub Release; release-please's notes and tag are kept.

| Target | Runtime baseline |
| --- | --- |
| `aarch64-apple-darwin` | Apple Silicon, macOS 15 or newer |
| `x86_64-unknown-linux-gnu` | Intel/AMD 64-bit, Ubuntu 24.04 or compatible newer system |
| `aarch64-unknown-linux-gnu` | ARM64, Ubuntu 24.04 or compatible newer system |

Linux binaries use system glibc, libstdc++, OpenSSL 3 and CA certificates.
Model weights download on first use; they are not included in the archives.
Intel macOS is not built because the pinned ONNX Runtime crate has no
prebuilt library for that target.
