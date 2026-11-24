# Configuration and operation

[Back to the README](../README.md) · [Usage](usage.md) · [Architecture](architecture.md)

recherche works without a configuration file. Create one when you want to
change source locations, ranking settings, the listening address, or optional
API features. Every setting is optional; omitted values use defaults from
[`src/config.rs`](../src/config.rs).

## File locations

recherche uses XDG locations on every supported OS, including macOS:

| Purpose | Default | Override |
| --- | --- | --- |
| Configuration | `~/.config/recherche/config.toml` | `$XDG_CONFIG_HOME/recherche/config.toml`, or `--config <path>` |
| Service environment | `~/.config/recherche/.env` | `$XDG_CONFIG_HOME/recherche/.env` (`chmod 600`) |
| Evaluation queries | `~/.config/recherche/eval.toml` | `$XDG_CONFIG_HOME/recherche/eval.toml`, or `eval --file <path>` |
| Archive, database, keyword index | `~/.local/share/recherche/` | `$XDG_DATA_HOME/recherche/`, or `RECHERCHE_DATA_DIR` for the complete data path |
| Downloaded models | `~/.cache/recherche/models/` | `$XDG_CACHE_HOME/recherche/models/` |
| Claude Code root | `~/.claude/` | `CLAUDE_CONFIG_DIR`, or `sources.claude.dir` |
| Codex root | `~/.codex/` | `CODEX_HOME`, or `sources.codex.dir` |
| Antigravity root | `~/.gemini/antigravity-cli/` | `sources.antigravity.dir` |
| Cursor root | `~/.cursor/` | `sources.cursor.dir` |

An explicit source setting takes precedence over its environment variable.
`RECHERCHE_DATA_DIR` takes precedence over `XDG_DATA_HOME`. Selecting a different
config file does not relocate data or the default evaluation file.

To create a config from the checkout:

```sh
mkdir -p "${XDG_CONFIG_HOME:-$HOME/.config}/recherche"
cp config.example.toml "${XDG_CONFIG_HOME:-$HOME/.config}/recherche/config.toml"
```

Use absolute paths in TOML source settings; the application does not expand
`~` in those strings. For example:

```toml
[sources]
poll_seconds = 30

[sources.claude]
dir = "/absolute/path/to/claude-data"

[sources.codex]
dir = "/absolute/path/to/codex-data"

[sources.antigravity]
dir = "/absolute/path/to/antigravity-cli"

[sources.cursor]
dir = "/absolute/path/to/cursor"
```

Restart `serve` after changing configuration or environment variables.

### Headless sessions

Scripts that run an agent non-interactively leave session files too, often
dozens with the same one-line prompt. By default recherche archives them but
leaves them out of search and the recent list:

```toml
[sources.claude]
exclude_entrypoints = ["sdk-cli"]   # `claude -p`

[sources.codex]
exclude_sources = ["exec"]          # `codex exec`
```

Claude Code records an `entrypoint` on each line of a session, and Codex
records a `source` in the session's first line. A Claude session is left out
only when every entrypoint in it is listed, so a terminal session later
continued with `claude -p --resume` stays. Sessions without an entrypoint
are always kept. Set a list to `[]` to index everything.

The lists apply when a session file is parsed. After changing them, run
`recherche index --rebuild` so already indexed sessions are re-evaluated.

### System instructions in prompts

Some harnesses, such as Conductor, prepend `<system_instruction>` blocks to
the prompts they send. To hide them in the web UI and the JSON API, including
titles taken from such a prompt:

```toml
[ui]
hide_system_instructions = true
```

It is off by default. Only what the UI and API return changes: the blocks stay
in the index and in summaries, and no rebuild is needed.

## Local search models

The default search settings enable both semantic retrieval and reranking:

```toml
[search]
vectors = true
rerank = true
rerank_model = "jina-turbo"
rerank_top = 20
model_idle_seconds = 300
boilerplate_min_sessions = 4
```

The embedder is BGE Small English v1.5. `rerank_model` can be `jina-turbo` or
`bge-base`; the latter is larger and slower. Models run locally on the CPU and
download on first use. The daemon unloads models after inactivity, so the next
search may need to load them again.

For keyword search without loading or downloading these models, set both
`vectors = false` and `rerank = false`. This also disables document embedding
during import. You can leave optional LLM features off independently:

```toml
[llm]
enabled = false
```

## Optional LLM features

The default configuration enables API features when `OPENROUTER_API_KEY` is
present and nonempty (from your shell environment or
`~/.config/recherche/.env`). Without it, local search remains available. The
defaults are:

```toml
[llm]
enabled = true
base_url = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_API_KEY"
model = "deepseek/deepseek-v4.1-flash"
rewrite_model = ""
deny_data_collection = true
rewrite = true
summaries = true
summarize_sources = ["claude", "codex", "antigravity", "cursor"]
summarize_exclude_paths = []
idle_minutes = 20
```

`model` selects the summary model and, unless `rewrite_model` is set, the query
expansion model. `base_url` can point to another OpenAI-compatible API. The
client appends `/chat/completions` to that URL. Cost estimation additionally
expects an OpenRouter-style `/models` response with pricing; other providers
may not supply one.

API calls can include sensitive content from your sessions:

| Feature | Request contents | How to disable it |
| --- | --- | --- |
| Query expansion | Original query and up to 80 known repository names | `rewrite = false` |
| Summaries | Agent, title, repo, working directory, selected entities and file names, prompts, errors, selected test-result lines, final assistant reply | `summaries = false` |

The summary digest is capped and does not submit the complete raw transcript,
but it is not a redaction mechanism. `deny_data_collection = true` sends
OpenRouter's request to use providers that do not collect prompt data. It is a
provider routing option, not a guarantee about an arbitrary API endpoint.

To summarize only one agent, use `summarize_sources = ["codex"]`, for example.
To exclude directories from summaries:

```toml
[llm]
summarize_exclude_paths = ["/private-projects/", "/confidential/"]
```

These are substring checks against each session's recorded working directory,
not globs. They apply to summaries, including `--redo`; they do not exclude
sessions from local ingestion, indexing, or the repository list sent for query
expansion. Disable rewrite separately if you do not want that request sent.

recherche does not track or cap API spend. Set credit or spending limits with
your provider. See [the usage guide](usage.md#optional-summaries-and-query-expansion)
before backfilling summaries.

### Credentials for background services (`brew services` and `systemd`)

Services started by `brew services` or `install-service` do not inherit your
shell environment, and `brew upgrade` / `brew services restart` regenerates
Homebrew's LaunchAgent plist. To pass `OPENROUTER_API_KEY` (or service
overrides like `RECHERCHE_LOG`) without editing service plists or putting
secrets in `config.toml`, place a `0600`-permissioned `.env` file in the config
directory:

```sh
mkdir -p "${XDG_CONFIG_HOME:-$HOME/.config}/recherche"
cp .env.example "${XDG_CONFIG_HOME:-$HOME/.config}/recherche/.env"
chmod 600 "${XDG_CONFIG_HOME:-$HOME/.config}/recherche/.env"
```

Then uncomment and set `OPENROUTER_API_KEY` in that file, restart the service
(`brew services restart recherche`), and verify with `recherche doctor`:

```dotenv
OPENROUTER_API_KEY=sk-or-...
```

At startup (except for `recherche demo`), recherche loads
`${XDG_CONFIG_HOME:-$HOME/.config}/recherche/.env` via `dotenvy`:

- Variables already set in your shell environment (for example via `direnv` or
  `op run`) take precedence and are never overwritten.
- On Unix, recherche skips the file and logs a warning if it is readable by
  group or others (`chmod 600` required).
- Restart `serve` after editing `.env` or rotating the key.

### Credentials with 1Password and direnv

The config stores the API key's environment variable name, not the secret.
recherche reads that variable from its process environment (including
`~/.config/recherche/.env` when present) and does not read `.envrc` itself.

If you use 1Password CLI and direnv, create a **1Password Environment** containing
`OPENROUTER_API_KEY` (or the name in `api_key_env`). Keep its real ID and secret
values out of tracked files. In a gitignored `.envrc`, load its variables with:

```bash
eval "$(direnv dotenv bash <(op environment secret list ENVIRONMENT_ID --reveal --format=dotenv))"
```

Replace `ENVIRONMENT_ID` in that local file, then run `direnv allow` yourself
after editing it. The `--reveal` option is required to load the actual values.
With direnv's shell hook enabled and 1Password authorized, start recherche from
that shell. After rotating the key, run `direnv reload` and restart recherche.

For credentials you want to expose only to a single process, use:

```sh
op run --environment ENVIRONMENT_ID -- recherche serve
```

`op run --environment` requires the Environment's ID. Do not put the secret
itself on the command line. A process started in a direnv shell keeps its
inherited key even after you leave the directory.

## Run in the background

With Homebrew, let `brew services` manage it:

```sh
brew services start recherche
```

The service runs `recherche serve` from Homebrew's `opt` path, so it keeps
working across `brew upgrade`; run `brew services restart recherche` after an
upgrade to pick up the new binary. Output goes to
`$(brew --prefix)/var/log/recherche.log`. Stop it with
`brew services stop recherche`.

Without Homebrew, install the binary at a stable path and run:

```sh
recherche install-service
```

This writes a service definition and prints the commands to activate it. It
does not start the service itself.

| OS | Generated file |
| --- | --- |
| macOS | `~/Library/LaunchAgents/dev.recherche.serve.plist` |
| Linux | `~/.config/systemd/user/recherche.service` |

The definition runs the current binary's absolute path with `serve`. If you
move the binary, regenerate the definition and reload the service. The macOS
definition writes stdout and stderr to `serve.log` in the default data directory;
Linux output is available through the user service's journal.

Services do not inherit your shell environment, whether started by
`brew services` or from `install-service`.
Put `OPENROUTER_API_KEY` (and any other environment overrides) in
`~/.config/recherche/.env` with `chmod 600`; see
[Credentials for background services](#credentials-for-background-services-brew-services-and-systemd).
`install-service` does not preserve a `--config` argument from the installation
command.

## Listening address

The default is `127.0.0.1:7777`. Override it with `listen` in TOML or
`recherche serve --listen <address>`. A non-loopback listener requires a
configured `token`; startup fails without one. Open the UI once with
`?token=<configured-token>` to set the authentication cookie.

This is a single shared-token interface to your archive. For access from your
other machines, keep it on a private network such as a Tailscale tailnet. The
application serves HTTP directly. On loopback, the token check is bypassed even
if a token is configured.

## Local data and backups

The data directory contains raw session copies under `archive/`, normalized
data in `recherche.db`, and the derived Tantivy index under `tantivy/`. Neither
the raw archive nor the database is redacted. Long tool output may be clipped
in the normalized transcript; the raw archived file retains the imported bytes.

Imported sessions stay in recherche when an agent removes its originals.
There is currently no CLI command for retention policies or selective deletion.
The archive only preserves files recherche has had a chance to import.

Back up the whole data directory, plus your configuration and `eval.toml`.
Stop recherche processes first for a simple consistent filesystem copy;
SQLite can have accompanying WAL files while it is running. Preserve the
database because it also contains archive tracking, generated summaries, and
evaluation feedback. The model cache can be downloaded again.

Keep real session exports, local configuration, evaluation queries, databases,
logs, and backups outside this repository. The ignore rules cover common local
artifacts, but cannot redact content pasted into source files or documentation.
Use invented prompts, paths, repository names, and session IDs in examples,
fixtures, screenshots, and PR descriptions. Check commit messages and PR bodies
for private session links before publishing; deleting a file or editing the
latest version does not remove earlier copies from Git history.

`recherche index --rebuild` reparses archives tracked in the database and
rebuilds keyword documents, including for sessions whose originals no longer
exist. It does not reconstruct a missing database from an untracked archive
directory. See [storage boundaries](architecture.md#storage-and-recovery-boundaries).

## Troubleshooting

Start with `recherche doctor`. For more logging:

```sh
RECHERCHE_LOG=recherche=debug recherche serve
```

| Symptom | What to check |
| --- | --- |
| No sessions appear | Check source roots in `doctor`; only JSONL files in the supported subdirectories are discovered |
| New text matches keywords but not meaning | Let the initial embedding pass or next background embedding batch finish; inspect logs for model download failures |
| Semantic search or reranking is unavailable | Check model download access and the cache path; keyword results remain usable when a model fails |
| Malformed-line counts are nonzero | `doctor` reports complete lines that are invalid JSON; an unfinished trailing line waits for the writer to finish |
| A deleted worktree's resume command fails | The transcript can survive in recherche, while the working directory and agent's own resume data may be gone |
| No generated kind or outcome | Those fields need optional summaries; use `summarize --dry-run` to inspect eligible sessions |
| LLM features work in a terminal but not as a service | Put `OPENROUTER_API_KEY` in `~/.config/recherche/.env` (`chmod 600`), restart the service, and check `recherche doctor` |
| `open` launches a page that cannot connect | `open` only opens a URL; start `serve` separately |

Source formats can evolve. Malformed-line counts detect invalid JSON, not every
unknown event type. If a valid session is missing content, inspect the relevant
source parser and use `index --rebuild` after a parser fix.
