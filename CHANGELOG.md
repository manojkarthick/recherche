# Changelog

## [0.6.1](https://github.com/manojkarthick/recherche/compare/v0.6.0...v0.6.1) (2026-11-26)


### Bug Fixes

* hide &lt;system_instruction&gt; blocks by default ([#47](https://github.com/manojkarthick/recherche/issues/47)) ([e535e82](https://github.com/manojkarthick/recherche/commit/e535e82a9005e30402dda0259bde170c0e426a40))

## [0.6.0](https://github.com/manojkarthick/recherche/compare/v0.5.0...v0.6.0) (2026-11-24)


### Features

* index Cursor sessions ([#46](https://github.com/manojkarthick/recherche/issues/46)) ([c71bb05](https://github.com/manojkarthick/recherche/commit/c71bb0557f477cc47adb3679db68a7d6838e5bba))
* optionally hide &lt;system_instruction&gt; blocks in prompts ([#45](https://github.com/manojkarthick/recherche/issues/45)) ([feffca4](https://github.com/manojkarthick/recherche/commit/feffca4ea31f15756b634ef6d74385270d333cad))
* **web:** use Geist and Geist Mono as the UI fonts ([#43](https://github.com/manojkarthick/recherche/issues/43)) ([430de85](https://github.com/manojkarthick/recherche/commit/430de850a65bc4e7febc9974593729706ba90296))

## [0.5.0](https://github.com/manojkarthick/recherche/compare/v0.4.0...v0.5.0) (2026-11-22)


### Features

* index Antigravity sessions ([#42](https://github.com/manojkarthick/recherche/issues/42)) ([2c2a3b6](https://github.com/manojkarthick/recherche/commit/2c2a3b640ca988495e84dc9ed965f81c9ac1bb92))


### Bug Fixes

* **web:** give the session page buttons one size and one right edge ([#40](https://github.com/manojkarthick/recherche/issues/40)) ([338d028](https://github.com/manojkarthick/recherche/commit/338d0283039e5b0802d7691a64e588de3f19a597))

## [0.4.0](https://github.com/manojkarthick/recherche/compare/v0.3.0...v0.4.0) (2026-11-22)


### ⚠ BREAKING CHANGES

* **config:** skip headless claude -p and codex exec sessions ([#34](https://github.com/manojkarthick/recherche/issues/34))

### Features

* **config:** load service credentials from a protected env file ([#38](https://github.com/manojkarthick/recherche/issues/38)) ([5b2dbde](https://github.com/manojkarthick/recherche/commit/5b2dbde4cc96f97d30c7f0805db042fc1b6faf29))
* **config:** skip headless claude -p and codex exec sessions ([#34](https://github.com/manojkarthick/recherche/issues/34)) ([a4c4a1a](https://github.com/manojkarthick/recherche/commit/a4c4a1a970990518d19cf2b537c2526cf11833af))
* **ingest:** read Codex sessions compressed to .jsonl.zst ([#36](https://github.com/manojkarthick/recherche/issues/36)) ([c478e73](https://github.com/manojkarthick/recherche/commit/c478e73a75a9f4f57bbbcc20dfe2ec332cb30650))
* **web:** add on-demand session reindexing and summaries ([#37](https://github.com/manojkarthick/recherche/issues/37)) ([b482dc4](https://github.com/manojkarthick/recherche/commit/b482dc4e3d838b57c8228f43455eac562019b4d8))


### Bug Fixes

* correct semantic search and summarization failures ([#39](https://github.com/manojkarthick/recherche/issues/39)) ([2b11dec](https://github.com/manojkarthick/recherche/commit/2b11decdf894007190c901e0131aafa4ec3b75aa))

## [0.3.0](https://github.com/manojkarthick/recherche/compare/v0.2.0...v0.3.0) (2026-11-20)


### Features

* **cli:** format list as a date-grouped timeline with filters ([#30](https://github.com/manojkarthick/recherche/issues/30)) ([84c8b42](https://github.com/manojkarthick/recherche/commit/84c8b42ba0939705f1a5afc4a0396e1786cd171a))
* **cli:** format search as ranked cards and quiet CLI info logs ([#33](https://github.com/manojkarthick/recherche/issues/33)) ([2039613](https://github.com/manojkarthick/recherche/commit/20396137fbb2e4a94abd4866e2878c6daf4b0ff4))

## [0.2.0](https://github.com/manojkarthick/recherche/compare/v0.1.0...v0.2.0) (2026-11-16)


### Features

* **brew:** run recherche serve with brew services ([#28](https://github.com/manojkarthick/recherche/issues/28)) ([846795a](https://github.com/manojkarthick/recherche/commit/846795a37b884803d1904993ab4df067d69a7bec))
* **daemon:** name the sessions each index, embed and summary pass touches ([#25](https://github.com/manojkarthick/recherche/issues/25)) ([a1f2fda](https://github.com/manojkarthick/recherche/commit/a1f2fda11f93869ff1619c491c576eaa00c8b2c3))
* **demo:** serve made-up sessions for UI work and screenshots ([#26](https://github.com/manojkarthick/recherche/issues/26)) ([37bcabd](https://github.com/manojkarthick/recherche/commit/37bcabdf33379e892448a8a09d220008d24f0bfe))
* **ingest:** recognize Codex, Herdr and Conductor worktree layouts ([#21](https://github.com/manojkarthick/recherche/issues/21)) ([94cd201](https://github.com/manojkarthick/recherche/commit/94cd201007e5934cae7646b32320e6ad0d59d0be))
* **web:** explain why a session has no summary yet ([#24](https://github.com/manojkarthick/recherche/issues/24)) ([6d9db74](https://github.com/manojkarthick/recherche/commit/6d9db7481c25f88c30cc7b883bc5db4bdc526f3d))
* **web:** restyle the UI as a noir dev tool ([#23](https://github.com/manojkarthick/recherche/issues/23)) ([ba53d31](https://github.com/manojkarthick/recherche/commit/ba53d3186bc69c1c64036c6f7cd7e84ff9746555))


### Bug Fixes

* **extract:** stop file contents and search output from naming repos and PRs ([#27](https://github.com/manojkarthick/recherche/issues/27)) ([bb7742c](https://github.com/manojkarthick/recherche/commit/bb7742cbefccc7be10647d1a1c086a8ce314a632))

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
