#!/usr/bin/env bash
# Build realistic Rust project trees from crates in the local cargo registry.
# Each fixture: proj/{Cargo.toml, src/main.rs, vendor/<crate sources>}
set -euo pipefail

BENCH=/tmp/opencode/bench
FIX=$BENCH/fixtures

# name, target files, target bytes (approx)
SPECS=(
    "small 200 2000000"
    "medium 4000 50000000"
    "large 15000 200000000"
)

# Collect registry source dirs
mapfile -t REGISTRY < <(find "$HOME/.cargo/registry/src" -mindepth 2 -maxdepth 2 -type d | sort)

if [[ ${#REGISTRY[@]} -lt 50 ]]; then
    echo "not enough crates in registry cache" >&2
    exit 1
fi

make_fixture() {
    local name=$1 want_files=$2 want_bytes=$3
    local root=$FIX/$name/proj
    rm -rf "$FIX/$name"
    mkdir -p "$root/src" "$root/vendor"
    cat > "$root/Cargo.toml" <<EOF
[package]
name = "bench-$name"
version = "0.1.0"
edition = "2021"
EOF
    echo 'fn main() {}' > "$root/src/main.rs"

    local files=0 bytes=0 i=0
    while (( files < want_files || bytes < want_bytes )) && (( i < ${#REGISTRY[@]} )); do
        local crate=${REGISTRY[$i]}
        i=$((i + 1))
        cp -a "$crate" "$root/vendor/"
        local cf cb
        cf=$(find "$root/vendor/$(basename "$crate")" -type f | wc -l)
        cb=$(du -sb "$root/vendor/$(basename "$crate")" | cut -f1)
        files=$((files + cf))
        bytes=$((bytes + cb))
    done
    echo "$name: $files files, $((bytes / 1048576)) MiB, $i crates"
}

for spec in "${SPECS[@]}"; do
    make_fixture $spec
done
