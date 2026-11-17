#!/usr/bin/env bash
set -euo pipefail

if [ $# -eq 0 ]; then
    echo "usage: $0 CHAIN..." >&2
    echo "  CHAIN is issue ids joined by '>' in order, e.g. 620\>622\>621\>623" >&2
    echo "  each issue depends on the one before it; ids are the server's issue numbers" >&2
    exit 2
fi

url="${AGENTBENCH_URL:-http://127.0.0.1:7420}"

for chain in "$@"; do
    prev=""
    IFS='>' read -ra ids <<< "$chain"
    for id in "${ids[@]}"; do
        if [ -n "$prev" ]; then
            curl -fsS -X PUT "$url/api/issues/$id/deps" \
                -H "Authorization: Bearer $AGENTBENCH_TOKEN" \
                -H 'Content-Type: application/json' \
                -d "[$prev]" > /dev/null
            echo "#$id depends on #$prev"
        fi
        prev="$id"
    done
done
