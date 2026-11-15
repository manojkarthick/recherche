# Agent notes

## UI work and screenshots

Use `cargo run -- demo`, not `serve`, to see UI changes or take screenshots.
It serves made-up sessions from `demo/sessions/` on http://127.0.0.1:7778, so
screenshots and PR descriptions never show real work. When a UI case isn't
covered (a badge, a long title, a missing directory), add a fixture; the format
is in `demo/README.md`.

## Don't touch real data

`demo` keeps everything under its own directory. Any other command (`index`,
`search`, `serve`, `doctor`) reads the real Claude Code and Codex sessions and
writes to the real index unless `RECHERCHE_DATA_DIR`, `CLAUDE_CONFIG_DIR` and
`CODEX_HOME` all point at a scratch directory.

## Before pushing

Run what CI runs:

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

## Summaries

- A fixture's `kind` and `outcome` must be values from `llm::KINDS` and
  `llm::OUTCOMES`; the summarizer never produces anything else.
- The UI explains a missing summary (in progress, queued, not summarized
  automatically) only when summaries are on and an API key is set. To see
  those notes without calling an API, keep summaries on and point
  `llm.base_url` at a closed local port, as `demo` does.
