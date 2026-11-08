#!/usr/bin/env bash
# English in, Turkish out (or --to en for the other way) through the codex CLI: the OpenAI model it
# runs writes more natural Turkish than Claude does, and this box has codex logged in already.
# Usage: translate.sh [--to tr|en] [--note "register, audience, terms to keep"] < text > translation
# Exit status is codex's: non-zero means no translation was produced (not logged in, no network, ...).
set -euo pipefail
to=tr note=
while [ $# -gt 0 ]; do
  case $1 in
    --to) to=$2; shift 2 ;;
    --note) note=$2; shift 2 ;;
    *) echo "usage: translate.sh [--to tr|en] [--note '...'] < text" >&2; exit 2 ;;
  esac
done
case $to in tr) lang=Turkish ;; en) lang=English ;; *) echo "translate.sh: --to tr|en" >&2; exit 2 ;; esac
out=$(mktemp) err=$(mktemp); trap 'rm -f "$out" "$err"' EXIT
# -o keeps only the final answer; --ephemeral saves no session; read-only sandbox: it must not touch files.
# codex narrates on stderr (banner, transcript, sandbox warnings): shown only when it fails.
codex exec --skip-git-repo-check -s read-only --ephemeral -o "$out" "Translate the text in the <stdin> block into natural, fluent $lang, the way a careful native writer would put it rather than word for word. Keep the markdown structure, code, commands, URLs, placeholders such as {name}, product and person names, and numbers exactly as they are. Translate everything, leave nothing out, add nothing. Output only the translation.${note:+ Notes from the author: $note}" >/dev/null 2>"$err" || { st=$?; tail -n 5 "$err" >&2; exit $st; }
cat "$out"; [ -z "$(tail -c1 "$out")" ] || echo   # -o writes no final newline
