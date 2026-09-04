#!/usr/bin/env bash
set -euo pipefail

# Compile Linux binaries, atomically install them into .data/bin, then
# perform a controlled restart through `zork update`. Does not
# rebuild Docker images or recreate the container.

root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root"

mkdir -p .data/bin

docker compose --profile tools run --rm --no-deps rust-build bash -c '
  set -euo pipefail
  bash /src/scripts/dev/release-build.sh
'
if docker compose ps -q zork | grep -q .; then
  docker compose exec zork /data/bin/zork update --data /data
else
  echo "zork is not running; binaries installed to .data/bin"
fi
