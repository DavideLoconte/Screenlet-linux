#!/bin/sh
# Runs a command in the build container (Containerfile): Rust and the GNOME
# libraries stay there. ./tools/dev.sh cargo test; DOCKER=docker to use docker.
set -e
cd "$(dirname "$0")/.."
engine="${DOCKER:-podman}"
"$engine" image exists screenlet-gnome 2>/dev/null || "$engine" build -q -t screenlet-gnome -f Containerfile .
z=""; [ "$engine" = podman ] && z=":Z"
exec "$engine" run --rm -v "$PWD:/src$z" -v screenlet-cargo:/cargo$z -w /src screenlet-gnome "$@"
