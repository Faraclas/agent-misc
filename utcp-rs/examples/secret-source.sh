#!/bin/bash
# Example secrets command for utcp. Copy it, adjust SECRETS_FILE and ALLOW, and point
# `[secrets] command` in ~/.config/utcp/settings.toml at the copy:
#
#   [secrets]
#   command = ["/bin/bash", "~/scripts/utcp-secret-source.sh"]
#
# utcp runs it with the variable names it wants as arguments (every name a registered manual may
# read) and reads NAME=value lines from its stdout. This one sources a shell file you already keep
# and prints only the requested names that are also in ALLOW, so the file's other secrets never
# reach utcp. utcp takes each value literally, up to the end of its line.
#
# Its stdout is the secret itself: don't run it in a terminal to check it. Run `utcp doctor`
# instead, which shows which names it printed (never the values).

SECRETS_FILE="$HOME/.bash_secrets"
ALLOW="GITHUB_TOKEN CONTEXT7_API_KEY"

if [[ ! -r "$SECRETS_FILE" ]]; then
    echo "cannot read $SECRETS_FILE" >&2
    exit 1
fi

# Only values from the file count, not ones this process inherited.
for name in $ALLOW; do
    unset "$name"
done
# shellcheck source=/dev/null
source "$SECRETS_FILE" >/dev/null

for name in "$@"; do
    [[ " $ALLOW " == *" $name "* ]] || continue
    value="${!name-}"
    if [[ -n "$value" ]]; then
        printf '%s=%s\n' "$name" "$value"
    fi
done
