# Changelog

## 0.1.0 (2026-11-14)


### Features

* add force flag for summary regeneration ([5080358](https://github.com/manojkarthick/recherche/commit/508035801d91dde516c61100c196576cbf93bcae))
* add recherche, local search for Claude Code and Codex sessions ([38fb525](https://github.com/manojkarthick/recherche/commit/38fb5251ea89f1909ce850331f845cf5105772dd))
* **api:** add JSON access to sessions and search ([#17](https://github.com/manojkarthick/recherche/issues/17)) ([965ef53](https://github.com/manojkarthick/recherche/commit/965ef5300031374ef79e808c0d8553333684eff0))
* **cli:** add session listing with time filters and JSON output ([#14](https://github.com/manojkarthick/recherche/issues/14)) ([99b1e5b](https://github.com/manojkarthick/recherche/commit/99b1e5bcdd1202893d575c5f1c540c22785dfd7c))
* **cli:** replace summarize --all/--force/--limit with --redo [N] ([326feab](https://github.com/manojkarthick/recherche/commit/326feab4cc94657f53a5e6e850dcd12d0e6c177f))
* display session summaries as plain bullets ([3bd7a03](https://github.com/manojkarthick/recherche/commit/3bd7a035df86d1c991f702fe2b201d205a1e9786))
* **llm:** drop spend tracking and price estimates from OpenRouter ([#10](https://github.com/manojkarthick/recherche/issues/10)) ([ab4079b](https://github.com/manojkarthick/recherche/commit/ab4079bc9756c06d470db15e913b0d61d1e47676))
* publish binaries and a Homebrew formula with JReleaser ([#19](https://github.com/manojkarthick/recherche/issues/19)) ([2b9e931](https://github.com/manojkarthick/recherche/commit/2b9e931514ac51795af2fe3f73f5e0845286952a))
* **web:** add visual hierarchy to search, session and entity pages ([#15](https://github.com/manojkarthick/recherche/issues/15)) ([8f1dcdf](https://github.com/manojkarthick/recherche/commit/8f1dcdf2d2451747bb8ea6fc344076da3a482cd4))
* **web:** distinguish repo and branch labels with colour ([#13](https://github.com/manojkarthick/recherche/issues/13)) ([f43c258](https://github.com/manojkarthick/recherche/commit/f43c258c17ff72c1cd74cbc6ff268223d2eaeec8))
* **web:** hide session file lists and add a favicon ([#18](https://github.com/manojkarthick/recherche/issues/18)) ([e682fe6](https://github.com/manojkarthick/recherche/commit/e682fe681ae9477f343827fe8b5d825ea1ac5492))
* **web:** link sidebar PRs, branches and repos to GitHub ([064ba3e](https://github.com/manojkarthick/recherche/commit/064ba3e77efb72815889f63420d7a2407170a837))
* **web:** render session transcripts as safe Markdown ([4855241](https://github.com/manojkarthick/recherche/commit/4855241826c0e526f559c62737b54b05a2ec0f7f))


### Bug Fixes

* make cargo test work on Amp orbs ([9e816f0](https://github.com/manojkarthick/recherche/commit/9e816f0118cffac8d1bb6b64b451f9b4e4bcc06d))
* satisfy clippy for CI ([d4ef110](https://github.com/manojkarthick/recherche/commit/d4ef110e2bfbf9759803abd320cbbb47b7cdfc06))
* set up Rust and compatible ONNX Runtime on orbs ([30bbab8](https://github.com/manojkarthick/recherche/commit/30bbab88e93cd5be1ac1f12b9fac42cf1eeea3ca))
* **summaries:** honor redo and refresh summaries without restarting ([#12](https://github.com/manojkarthick/recherche/issues/12)) ([c7fc268](https://github.com/manojkarthick/recherche/commit/c7fc268df0313b21f9f9f9e3de737b58323d56a3))


### Continuous Integration

* set up release-please ([#8](https://github.com/manojkarthick/recherche/issues/8)) ([13161ac](https://github.com/manojkarthick/recherche/commit/13161aca6f8b44adda8cee3f30f9d0461c016801))
