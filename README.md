# recherche

Search your past Claude Code and Codex sessions by what you remember: a few words, a ticket, a PR, a branch, or a sentence like "the thread where we fixed the ordering issue".

It runs on your machine as one binary. It imports every session, keeps its own copy of the files (Claude Code deletes old sessions), and indexes new ones as you work. Repos, branches, worktrees, PRs, tickets and agents are recovered from the session files alone, so sessions in deleted worktrees stay searchable.

## Use

```sh
cargo build --release
./target/release/recherche serve          # indexes, then serves http://127.0.0.1:7777
```

The first run imports everything and downloads two small local models (an embedder and a reranker, about 300 MB) into `$XDG_CACHE_HOME/recherche/models`.

In the web UI, type what you remember. Keyword results appear while you type; results ranked by meaning replace them a moment later. Press `/` to search, `j`/`k` to move, `Enter` to open, `c` to copy the resume command. "This was it" on a result adds it to your eval set.

You can also put filters in the query: `repo:dotfiles agent:codex since:30d branch:fix-login kind:bugfix outcome:fixed`. Phrases like "last week" or "in March" boost sessions from that time, and "in <repo>" filters to that repo.

Other commands:

| Command | What it does |
| --- | --- |
| `recherche search "<query>"` | The same search from the terminal; `--json` for scripts |
| `recherche open "<query>"` | Opens the web UI with that query |
| `recherche index [--rebuild]` | One import pass; `--rebuild` reparses everything from the archive |
| `recherche summarize --all --dry-run` | Shows how many existing sessions would be summarized and the estimated cost |
| `recherche eval [-v] [--rewrite]` | Scores ranking configurations against your eval queries |
| `recherche install-service` | Writes a launchd agent (macOS) or systemd user unit (Linux) for `serve` |
| `recherche doctor` | Paths, counts, unparsed lines and LLM spend |

## LLM features (optional)

With `OPENROUTER_API_KEY` set, two features call a model through [OpenRouter](https://openrouter.ai), DeepSeek V4.1 Flash by default:

- **Query rewrite**: for sentence-like queries whose top results score weakly, the query is expanded into terms the transcript may have used ("history tables" becomes "audit table, SCD2, temporal table"). The refined results stream in after the first ones; the reranker still scores against your own words.
- **Session summaries**: a title, a short summary, a kind and an outcome for each finished session, written from a capped digest of the session rather than the whole transcript. While `serve` runs, sessions that started after it first ran are summarized once they go idle. Run `recherche summarize --all` once for the backlog.

Any OpenRouter model works: set `llm.model`, and `llm.rewrite_model` to use a different (faster) model for rewrite. Any other OpenAI-compatible API works too through `llm.base_url`.

Spend is recorded from the cost OpenRouter reports for each call and capped at `llm.monthly_budget_usd` (default $5). At the cap, LLM features stop for the month and search keeps working. Requests ask OpenRouter to use only hosts that don't store or train on prompts (`llm.deny_data_collection`). Both Claude Code and Codex sessions are summarized by default; use `llm.summarize_sources` to limit this to one agent, and `llm.summarize_exclude_paths` to keep directories such as work repos out.

Search itself never calls an API: the embedder and reranker run locally.

### Load the API key with direnv and 1Password

Recherche reads the key from `OPENROUTER_API_KEY` by default. Set
`llm.api_key_env` in your config to use a different environment variable; the
config stores the variable's name, not the secret. Recherche does not read
`.envrc` itself.

With [direnv](https://direnv.net) installed and its hook enabled in your shell,
and the [1Password CLI](https://developer.1password.com/docs/cli/) installed and
signed in:

1. Create a **1Password Environment** containing `OPENROUTER_API_KEY` (or the
   variable named by `llm.api_key_env`). Store the secret there, not in a regular
   vault item or as a plain value in `.envrc`.
2. Copy `.envrc.example` to `.envrc` and replace `ENVIRONMENT_ID` with your
   Environment's ID. Keep the real ID only in this gitignored file, not in the
   tracked example.
3. Run `direnv allow` in this checkout after editing `.envrc` and approve any
   1Password prompt. Direnv loads the Environment using:

   ```bash
   eval "$(direnv dotenv bash <(op environment read ENVIRONMENT_ID))"
   ```

Then run commands normally, without an `op run` wrapper:

```sh
./target/release/recherche serve
```

The local `.envrc` and direnv's `.direnv/` directory are gitignored; only the
secret-free template is tracked. Direnv loads the key when you enter this
checkout and removes it from the shell environment when you leave. After
rotating the key, run `direnv reload` and restart recherche so it inherits the
new value. Do not print the key or enable shell tracing while loading it.

Scripts and Makefiles should only consume the exported variables and fail with
a clear message when a required variable is missing. They should not call `op`
or read `.envrc`. Recherche itself keeps LLM features optional: without the key,
local search still works.

### Background processes and services

`recherche serve` started from the direnv-enabled shell inherits the key, even
if you put it in the background. Leaving the directory does not remove the
key from an already-running process. A command launched elsewhere does not
automatically load this checkout's `.envrc`.

The launchd agent or systemd user unit generated by `recherche install-service`
starts the binary directly, without direnv or your interactive shell's
environment. Running `install-service` from a direnv-enabled shell does not
copy the key into the service. LLM features therefore remain off unless you
configure the service's environment separately. To use this `.envrc` with a
service, its launch command would need to run
`/absolute/path/to/direnv exec /absolute/path/to/recherche /absolute/path/to/recherche-binary serve`
instead. That also requires access to `op` and an authorized 1Password session
at service startup; desktop biometric approval may not be available during
unattended startup. This setup does not provide unattended service credentials.

## Config

`$XDG_CONFIG_HOME/recherche/config.toml`; every key is optional. See [config.example.toml](config.example.toml).

Data lives in `$XDG_DATA_HOME/recherche` (`RECHERCHE_DATA_DIR` overrides it): the session archive, SQLite and the search index. The index can always be rebuilt from the archive.

## Eval

Ranking changes should be measured. Put real queries in `$XDG_CONFIG_HOME/recherche/eval.toml`, mostly in the conversational form you actually type:

```toml
[[query]]
q = "the thread where we fixed the ordering issue"
class = "conversational"   # or exact, entity, temporal
expect = ["<session id>"]
```

`recherche eval` reports recall@5, recall@10 and MRR per class for keyword only, keyword plus vectors, and with the reranker (`--rewrite` adds the LLM rewrite). Results you mark with "This was it" are included as the `feedback` class.
