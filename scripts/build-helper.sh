#!/usr/bin/env sh
# Builds the Clef token-count helper (native/clef-token-count) into
# build/clef-token-count/, where pool-miner finds it when run from the repo.
# Needs CMake, a C++17 compiler and Git; no GPU toolkit.
#
#   LLAMA_CPP_SOURCE_DIR=/path/to/llama.cpp  reuse a checkout at the pinned commit
#   BUILD_DIR=...                           override the build directory
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
build=${BUILD_DIR:-"$root/build/clef-token-count"}

generator=""
if command -v ninja >/dev/null 2>&1; then
    generator="-G Ninja"
fi
source_arg=""
if [ -n "${LLAMA_CPP_SOURCE_DIR:-}" ]; then
    source_arg="-DLLAMA_CPP_SOURCE_DIR=$LLAMA_CPP_SOURCE_DIR"
fi

# shellcheck disable=SC2086
cmake -S "$root/native/clef-token-count" -B "$build" -DCMAKE_BUILD_TYPE=Release $generator $source_arg
cmake --build "$build" --target clef-token-count --config Release --parallel

binary="$build/clef-token-count"
[ -x "$binary" ] || binary="$build/Release/clef-token-count.exe"
"$binary" --version
echo "built $binary"
