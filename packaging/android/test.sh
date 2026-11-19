#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
K=${KOTLIN_HOME:-$HOME/.cache/agentbench-android/kotlinc}
OUT=$(mktemp -d)
"$K/bin/kotlinc" -nowarn crates/app/android/src/io/v0l/agentbench/Attention.kt crates/app/android/test/AttentionTest.kt -d "$OUT/t.jar"
java -cp "$OUT/t.jar:$K/lib/kotlin-stdlib.jar" io.v0l.agentbench.AttentionTestKt
rm -rf "$OUT"
