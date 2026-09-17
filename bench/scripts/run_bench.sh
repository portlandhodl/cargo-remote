#!/usr/bin/env bash
# Benchmark: per-file protocol (rsync/scp) vs streamed archive (tar family)
# over ssh, at different tree sizes and link latencies.
#
# Scenarios per (size, latency, mode):
#   cold   - remote dir is empty (first sync)
#   noop   - remote already in sync (cost of a no-change re-sync)
#   change - 1 file modified, 1 deleted, 1 added (typical edit-build cycle)
#
# Output CSV: size,latency,mode,scenario,wall_s,bytes,verify
set -u

BENCH=/tmp/opencode/bench
FIX=$BENCH/fixtures
REMOTE=$BENCH/remote/builds
SNAR_DIR=$BENCH/inc
SSH_CFG=$BENCH/ssh_config
CSV=$BENCH/results.csv

mkdir -p "$REMOTE" "$SNAR_DIR"
echo "size,latency,mode,scenario,wall_s,bytes,verify" > "$CSV"

now_ms() { date +%s%N; }

remote_cmd() { ssh -F "$SSH_CFG" "$1" "${@:2}"; }

wipe_remote() { remote_cmd "$1" "rm -rf '$2'"; }

local_list() { (cd "$1" && find . -type f ! -path './target/*' | LC_ALL=C sort); }

remote_list() { remote_cmd "$1" "cd '$2' && find . -type f ! -path './target/*' | LC_ALL=C sort"; }

verify() { # root, host, dir, allow_stale(0|1) -> OK / OK+target / STALE / FAIL
    local root=$1 host=$2 dir=$3 allow_stale=$4 r d
    r=$(remote_list "$host" "$dir") || { echo FAIL; return; }
    d=$(diff <(local_list "$root") <(echo "$r"))
    if [[ -z $d ]]; then
        if remote_cmd "$host" "test -e '$dir/target/debug/junk.bin'"; then
            echo "OK+target"   # copies target/ too (expected for scp-r only)
        else
            echo "OK"
        fi
    elif [[ $allow_stale == 1 ]] && ! grep -q '^<' <<< "$d"; then
        echo "STALE"           # only remote-side extras: deletions not propagated
    else
        echo "FAIL"
    fi
}

# Emit the transfer command. tar-family commands include an inline
# `tee >(wc -c > BYTESFILE)` measuring exact wire bytes (post-compression).
xfer_cmd() { # mode, root, host, dir, snar, bytesfile
    local mode=$1 root=$2 host=$3 dir=$4 snar=$5 bf=$6
    local ssh="ssh -F $SSH_CFG $host"
    case $mode in
        rsync)
            echo "rsync -a --delete --stats --rsync-path='mkdir -p $dir && rsync' --exclude /target/ -e 'ssh -F $SSH_CFG' $root/ $host:$dir/" ;;
        rsync-z)
            echo "rsync -az --delete --stats --rsync-path='mkdir -p $dir && rsync' --exclude /target/ -e 'ssh -F $SSH_CFG' $root/ $host:$dir/" ;;
        tar)
            echo "tar --exclude=./target -C $root -cf - . | tee >(wc -c > $bf) | $ssh 'mkdir -p \"$dir\" && tar -xf - -C \"$dir\"'" ;;
        tar-gz)
            echo "tar --exclude=./target -C $root -cf - . | gzip -c | tee >(wc -c > $bf) | $ssh 'mkdir -p \"$dir\" && gzip -dc | tar -xf - -C \"$dir\"'" ;;
        tar-zstd)
            echo "tar --exclude=./target -C $root -cf - . | zstd -c | tee >(wc -c > $bf) | $ssh 'mkdir -p \"$dir\" && zstd -dc | tar -xf - -C \"$dir\"'" ;;
        tar-inc)
            echo "tar --listed-incremental=$snar --exclude=./target -C $root -cf - . | tee >(wc -c > $bf) | $ssh 'mkdir -p \"$dir\" && tar -x --listed-incremental=/dev/null -f - -C \"$dir\"'" ;;
        scp-r)
            echo "$ssh 'mkdir -p \"$dir\"' && scp -q -r -F $SSH_CFG $root/. $host:$dir/" ;;
    esac
}

run_timed() { # mode, root, host, dir, snar -> "wall_s,bytes"
    local mode=$1 root=$2 host=$3 dir=$4 snar=$5
    local cmd out start end rc wall bytes=""
    local bf
    bf=$(mktemp)
    cmd=$(xfer_cmd "$mode" "$root" "$host" "$dir" "$snar" "$bf")
    start=$(now_ms)
    out=$(eval "$cmd" 2>&1)
    rc=$?
    end=$(now_ms)
    wall=$(python3 -c "print(f'{($end-$start)/1e9:.3f}')")
    if [[ $rc -ne 0 ]]; then echo "RUN-FAIL($mode): $(echo "$out" | tail -2)" >&2; fi
    case $mode in
        rsync|rsync-z)
            bytes=$(echo "$out" | grep -oP 'Total bytes sent: \K[0-9,]+' | tr -d ',' || true) ;;
        *)
            # process substitution may lag the pipeline slightly
            for _ in $(seq 20); do [[ -s $bf ]] && break; sleep 0.05; done
            bytes=$(cat "$bf" 2>/dev/null || true) ;;
    esac
    rm -f "$bf"
    echo "$wall,$bytes"
}

mutate_tree() { # root : modify 1 file, delete 1, add 1 (in place: inodes preserved)
    local root=$1 first victim
    first=$(cd "$root" && find vendor -type f -name '*.rs' | sort | sed -n '10p')
    echo "// edited $(date +%s)" >> "$root/$first"
    echo 'pub fn brand_new() {}' > "$root/vendor/brand_new_file.rs"
    victim=$(cd "$root" && find vendor -type f -name '*.rs' | sort | sed -n '40p')
    rm -f "$root/$victim"
}

restore_fixture() { # size : rsync from pristine (only mutated files get new inodes)
    rsync -a --delete "$FIX/.pristine-$1/proj/" "$FIX/$1/proj/"
}

# pristine masters
for size in small medium large; do
    [[ -d $FIX/.pristine-$size ]] || cp -a "$FIX/$size" "$FIX/.pristine-$size"
done

MODES_COLD="rsync rsync-z tar tar-gz tar-zstd tar-inc scp-r"
MODES_WARM="rsync rsync-z tar tar-gz tar-zstd tar-inc"

LAT_small="bench-direct bench-50ms bench-100ms"
LAT_medium="bench-direct bench-50ms bench-100ms"
LAT_large="bench-direct bench-50ms"

label() { case $1 in bench-direct) echo 0ms;; bench-50ms) echo 50ms;; bench-100ms) echo 100ms;; esac; }

SIZES=${SIZES:-"small medium large"}
for size in $SIZES; do
    root=$FIX/$size/proj
    latvar=LAT_$size
    for host in ${HOSTS:-${!latvar}}; do
        lat=$(label "$host")
        dir=$REMOTE/$size
        snar=$SNAR_DIR/$size.snar
        echo "=== $size @ $lat ===" >&2

        # ---- cold (scp only on small: per-file SFTP round trips make it
        # absurdly slow at latency; small demonstrates the point)
        local_cold=$MODES_COLD
        [[ $size != small ]] && local_cold="rsync rsync-z tar tar-gz tar-zstd tar-inc"
        for mode in $local_cold; do
            restore_fixture "$size"
            wipe_remote "$host" "$dir"
            [[ $mode == tar-inc ]] && rm -f "$snar"
            res=$(run_timed "$mode" "$root" "$host" "$dir" "$snar")
            v=$(verify "$root" "$host" "$dir" 0)
            echo "$size,$lat,$mode,cold,$res,$v" >> "$CSV"
            echo "  $mode cold: $res $v" >&2
        done

        # ---- warm (per mode, continuing from a fresh prime)
        for mode in $MODES_WARM; do
            restore_fixture "$size"
            wipe_remote "$host" "$dir"
            [[ $mode == tar-inc ]] && rm -f "$snar"
            eval "$(xfer_cmd "$mode" "$root" "$host" "$dir" "$snar" /dev/null)" > /dev/null 2>&1  # prime

            res=$(run_timed "$mode" "$root" "$host" "$dir" "$snar")
            v=$(verify "$root" "$host" "$dir" 0)
            echo "$size,$lat,$mode,noop,$res,$v" >> "$CSV"
            echo "  $mode noop: $res $v" >&2

            mutate_tree "$root"
            res=$(run_timed "$mode" "$root" "$host" "$dir" "$snar")
            stale=0; [[ $mode == tar || $mode == tar-gz || $mode == tar-zstd ]] && stale=1
            v=$(verify "$root" "$host" "$dir" "$stale")
            echo "$size,$lat,$mode,change,$res,$v" >> "$CSV"
            echo "  $mode change: $res $v" >&2
        done
        wipe_remote "$host" "$dir"
    done
    restore_fixture "$size"
done

echo "done -> $CSV" >&2
