#!/usr/bin/env bash
# Run from the extracted archive, with no access to the user's session data.
set -euo pipefail
binary=$1
version=$2
[[ "$binary" = /* ]] || binary="$PWD/$binary"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
export HOME="$work/home"
export XDG_CONFIG_HOME="$work/config"
export XDG_DATA_HOME="$work/data"
export XDG_CACHE_HOME="$work/cache"
export CLAUDE_CONFIG_DIR="$work/claude"
export CODEX_HOME="$work/codex"
export RECHERCHE_LOG=recherche=debug
mkdir -p "$HOME" "$CLAUDE_CONFIG_DIR/projects/example" "$CODEX_HOME"
cd "$work"
[[ "$("$binary" --version)" = "recherche $version" ]]
"$binary" --help > /dev/null
if [[ "$(uname)" = Darwin ]]; then
  otool -L "$binary" | tail -n +2 > linkage.txt
  if grep -E '/Users/|/opt/homebrew/|/usr/local/|/target/' linkage.txt; then
    echo 'Binary links to build-machine libraries' >&2
    exit 1
  fi
else
  ldd "$binary" > linkage.txt
  if grep -E 'not found|/home/runner/|/target/' linkage.txt; then
    echo 'Binary has missing or build-machine libraries' >&2
    exit 1
  fi
fi
cat > config.toml <<'TOML'
[llm]
enabled = false
TOML
cat > "$CLAUDE_CONFIG_DIR/projects/example/smoke.jsonl" <<'JSONL'
{"type":"user","sessionId":"release-smoke","timestamp":"2026-01-01T00:00:00Z","cwd":"/example","message":{"content":"How do I search archived conversations with local semantic models?"}}
{"type":"assistant","sessionId":"release-smoke","timestamp":"2026-01-01T00:00:01Z","message":{"content":[{"type":"text","text":"Use recherche to search archived conversations with keyword retrieval, local embeddings and a reranker."}]}}
JSONL
"$binary" --config "$work/config.toml" search 'search archived conversations' > results.txt 2> models.log
cat models.log
cat results.txt
# The CLI can fall back to keyword search; require successful model execution.
grep -Eq 'embedded [1-9][0-9]*/[1-9][0-9]*' models.log
grep -q 'reranked 1 sessions' models.log
if grep -E 'embedding failed|semantic search unavailable|reranker unavailable' models.log; then
  exit 1
fi
