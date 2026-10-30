#!/usr/bin/env bash
# Benchmark the original Zig Ghostty against the Rust port, apples to apples.
#
# Builds `ghostty-bench` and `ghostty-gen` on both sides (Zig ReleaseFast with
# -Dapp-runtime=none, Rust `--release --no-default-features`), generates the
# synthetic input corpora ONCE (with the Zig `ghostty-gen`), feeds the identical files to both `ghostty-bench` binaries
# under hyperfine, and prints a Markdown table (median wall time, stddev,
# Rust/Zig ratio). See docs/BENCHMARKS.md for method and results.
#
# Usage:
#   tools/bench_compare.sh --zig /path/to/zig [options]
#
# Options:
#   --zig PATH          Zig toolchain binary (0.17.0, required unless --skip-build)
#   --zig-src DIR       Zig Ghostty checkout (default: <repo>/ghostty, or the
#                       main checkout's ghostty/ when run from a worktree). It is
#                       copied to WORK and built there; the checkout is never
#                       written to.
#   --work DIR          Scratch directory (default: /tmp/ghostty-bench-compare)
#   --deps-dir DIR      Pre-downloaded Zig dependency tarballs. Needed when
#                       Zig's own fetcher cannot reach deps.files.ghostty.org
#                       (e.g. behind a CONNECT proxy). Each package is looked up
#                       as DIR/<basename of its URL>, DIR/<zig hash>.tar.gz or
#                       DIR/<zig hash>/ and unpacked into zig-pkg/<zig hash>.
#   --rust-native       Also build/run the Rust port with -C target-cpu=native
#                       (Zig builds for the native CPU by default).
#   --zig-baseline      Also build/run Zig with -Dcpu=baseline (same ISA as
#                       the default Rust build; also profilable with valgrind).
#   --skip-build        Reuse binaries already in WORK.
#   --skip-gen          Reuse data already in WORK/data.
#   --runs N            hyperfine runs per command (default 8; x10 for startup/)
#   --warmup N          hyperfine warmup runs (default 2)
#   --filter REGEX      Only run cases whose id matches REGEX
#   --jobs N            Build parallelism (default 2)
#   --mb N              Size of the main corpora in MiB (default 64)
#   --report-only       Only print the table from existing WORK/results
#
# Environment: HYPERFINE=/path/to/hyperfine (default: from PATH; without it a
# python timer with the same warmup/runs is used).
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
ZIG=""
ZIG_SRC="$ROOT/ghostty"
WORK=/tmp/ghostty-bench-compare
DEPS_DIR=""
RUST_NATIVE=0
ZIG_BASELINE=0
SKIP_BUILD=0
SKIP_GEN=0
RUNS=8
WARMUP=2
FILTER=""
JOBS=2
MB=64
REPORT_ONLY=0

while [ $# -gt 0 ]; do
    case "$1" in
        --zig) ZIG=$2; shift 2 ;;
        --zig-src) ZIG_SRC=$2; shift 2 ;;
        --work) WORK=$2; shift 2 ;;
        --deps-dir) DEPS_DIR=$2; shift 2 ;;
        --rust-native) RUST_NATIVE=1; shift ;;
        --zig-baseline) ZIG_BASELINE=1; shift ;;
        --skip-build) SKIP_BUILD=1; shift ;;
        --skip-gen) SKIP_GEN=1; shift ;;
        --runs) RUNS=$2; shift 2 ;;
        --warmup) WARMUP=$2; shift 2 ;;
        --filter) FILTER=$2; shift 2 ;;
        --jobs) JOBS=$2; shift 2 ;;
        --mb) MB=$2; shift 2 ;;
        --report-only) REPORT_ONLY=1; SKIP_BUILD=1; SKIP_GEN=1; shift ;;
        -h|--help) sed -n '2,/^set -euo/{/^set -euo/d;s/^# \{0,1\}//;p;}' "$0"; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done

mkdir -p "$WORK"
WORK=$(cd "$WORK" && pwd)
DATA=$WORK/data
RES=$WORK/results
mkdir -p "$DATA" "$RES"

ZB=$WORK/zig-out/bin
RB=$WORK/rust-target/release
RNB=$WORK/rust-target-native/release
ZBB=$WORK/zig-out-baseline/bin

log() { printf '\033[1m==> %s\033[0m\n' "$*" >&2; }

#---------------------------------------------------------------------------
# Build
#---------------------------------------------------------------------------

populate_zig_pkgs() {
    # Zig 0.17 looks for packages in <build root>/zig-pkg/<hash> before
    # fetching. Unpack pre-downloaded tarballs there, stripping a single
    # top-level directory like Zig does.
    python3 -I - "$WORK/ghostty-src" "$DEPS_DIR" <<'EOF'
import json, os, shutil, subprocess, sys, tempfile
src, deps = sys.argv[1], sys.argv[2]
manifest = json.load(open(os.path.join(src, "build.zig.zon.json")))
pkgdir = os.path.join(src, "zig-pkg")
os.makedirs(pkgdir, exist_ok=True)
for zhash, info in manifest.items():
    dest = os.path.join(pkgdir, zhash)
    if os.path.isdir(dest):
        continue
    url = info["url"]
    cands = [os.path.join(deps, os.path.basename(url.split("#")[0])),
             os.path.join(deps, zhash + ".tar.gz"),
             os.path.join(deps, zhash)]
    path = next((c for c in cands if os.path.exists(c)), None)
    if path is None:
        print(f"  (no local copy of {zhash}; zig will try to fetch it)")
        continue
    if os.path.isdir(path):
        shutil.copytree(path, dest, symlinks=True)
        continue
    tmp = tempfile.mkdtemp(dir=pkgdir, prefix=".tmp-")
    try:
        subprocess.check_call(["tar", "-xf", path, "-C", tmp],
                              stderr=subprocess.DEVNULL)
    except subprocess.CalledProcessError:
        print(f"  (cannot unpack {path}; zig will try to fetch it)")
        shutil.rmtree(tmp)
        continue
    entries = os.listdir(tmp)
    root = tmp
    if len(entries) == 1 and os.path.isdir(os.path.join(tmp, entries[0])):
        root = os.path.join(tmp, entries[0])
    shutil.move(root, dest)
    shutil.rmtree(tmp, ignore_errors=True)
EOF
}

build_zig() {
    [ -n "$ZIG" ] || { echo "--zig is required" >&2; exit 2; }
    if [ ! -f "$ZIG_SRC/build.zig" ]; then
        # In a linked git worktree, fall back to the main checkout's copy.
        local common
        common=$(git -C "$ROOT" rev-parse --path-format=absolute --git-common-dir 2>/dev/null || true)
        if [ -n "$common" ] && [ -f "$(dirname "$common")/ghostty/build.zig" ]; then
            ZIG_SRC=$(dirname "$common")/ghostty
        else
            echo "Zig Ghostty source not found at $ZIG_SRC (use --zig-src)" >&2
            exit 2
        fi
    fi
    log "Copying Zig source $ZIG_SRC -> $WORK/ghostty-src"
    mkdir -p "$WORK/ghostty-src"
    tar -C "$ZIG_SRC" --exclude=./.git --exclude=./zig-pkg \
        --exclude=./.zig-cache --exclude=./zig-out -cf - . |
        tar -C "$WORK/ghostty-src" -xf -
    if [ -n "$DEPS_DIR" ]; then
        log "Populating zig-pkg from $DEPS_DIR"
        populate_zig_pkgs
    fi
    log "Building Zig (ReleaseFast, -Demit-bench, -Dapp-runtime=none)"
    (cd "$WORK/ghostty-src" &&
        ZIG_GLOBAL_CACHE_DIR=$WORK/zig-global-cache "$ZIG" build -j"$JOBS" \
            --cache-dir "$WORK/zig-cache" --prefix "$WORK/zig-out" \
            -Doptimize=ReleaseFast -Demit-bench=true \
            -Dapp-runtime=none \
            -Demit-docs=false -Demit-themes=false)
    if [ "$ZIG_BASELINE" = 1 ]; then
        log "Building Zig with -Dcpu=baseline"
        (cd "$WORK/ghostty-src" &&
            ZIG_GLOBAL_CACHE_DIR=$WORK/zig-global-cache "$ZIG" build -j"$JOBS" \
                --cache-dir "$WORK/zig-cache" --prefix "$WORK/zig-out-baseline" \
                -Doptimize=ReleaseFast -Demit-bench=true -Dcpu=baseline \
                -Dapp-runtime=none \
                -Demit-docs=false -Demit-themes=false)
    fi
}

build_rust() {
    log "Building Rust (cargo --release --no-default-features)"
    (cd "$ROOT" && CARGO_TARGET_DIR=$WORK/rust-target CARGO_BUILD_JOBS=$JOBS \
        cargo build --release --no-default-features \
            --bin ghostty-bench --bin ghostty-gen)
    if [ "$RUST_NATIVE" = 1 ]; then
        log "Building Rust with -C target-cpu=native"
        (cd "$ROOT" && CARGO_TARGET_DIR=$WORK/rust-target-native \
            CARGO_BUILD_JOBS=$JOBS RUSTFLAGS="-C target-cpu=native" \
            cargo build --release --no-default-features \
                --bin ghostty-bench)
    fi
}

if [ "$SKIP_BUILD" = 0 ]; then
    build_zig
    build_rust
fi
[ -x "$RNB/ghostty-bench" ] && RUST_NATIVE=1
[ -x "$ZBB/ghostty-bench" ] && ZIG_BASELINE=1

#---------------------------------------------------------------------------
# Data (generated once, by the Zig generator, shared by both sides)
#---------------------------------------------------------------------------

gen() { # gen <file> <bytes> <ghostty-gen args...>
    local out=$1 bytes=$2; shift 2
    log "Generating $(basename "$out") ($bytes bytes): ghostty-gen $*"
    # ghostty-gen writes forever; head closes the pipe once it has enough.
    { "$ZB/ghostty-gen" "$@" || true; } | head -c "$bytes" >"$out"
    [ "$(stat -c %s "$out")" = "$bytes" ] || { echo "short corpus $out" >&2; exit 1; }
}

MiB=$((1024 * 1024))
if [ "$SKIP_GEN" = 0 ]; then
    gen "$DATA/ascii.vt" $((MB * MiB)) +ascii
    gen "$DATA/ascii-lines.vt" $((MB * MiB)) +ascii --line-min=0 --line-max=160
    gen "$DATA/utf8.txt" $((MB * MiB)) +utf8 --seed=1
    gen "$DATA/styled.vt" $((MB * MiB)) +styled --seed=42
    gen "$DATA/osc.vt" $((MB / 8 * MiB)) +osc --style=streaming
    gen "$DATA/osc.bin" $((MB / 2 * MiB)) +osc --style=parser
    gen "$DATA/kitty.apc" $((MB * MiB)) +kitty
    gen "$DATA/styled-4m.vt" $((4 * MiB)) +styled --seed=7
    gen "$DATA/styled-640k.vt" 640000 +styled --seed=42
    gen "$DATA/ascii-1m.vt" 1000000 +ascii
    log "Generating seq.txt (seq 1 N, a large cat-style log)"
    { seq 1 20000000 || true; } | head -c $((MB * 2 * MiB)) >"$DATA/seq.txt"
fi

#---------------------------------------------------------------------------
# Cases: id | ghostty-bench arguments (identical on both sides)
#---------------------------------------------------------------------------

D=$DATA
CASES=(
    # Full VT stream -> terminal state (the IO-thread hot path)
    "stream/ascii|+terminal-stream --data=$D/ascii.vt"
    "stream/ascii-lines|+terminal-stream --data=$D/ascii-lines.vt"
    "stream/seq|+terminal-stream --data=$D/seq.txt"
    "stream/utf8|+terminal-stream --data=$D/utf8.txt"
    "stream/styled|+terminal-stream --data=$D/styled.vt"
    "stream/osc|+terminal-stream --data=$D/osc.vt"
    "stream/kitty|+terminal-stream --data=$D/kitty.apc"
    # VT parser state machine only
    "parser/ascii|+terminal-parser --data=$D/ascii.vt"
    "parser/utf8|+terminal-parser --data=$D/utf8.txt"
    "parser/styled|+terminal-parser --data=$D/styled.vt"
    # OSC / APC protocol parsers
    "osc-parser|+osc-parser --data=$D/osc.bin"
    "apc-parser/kitty|+apc-parser --data=$D/kitty.apc"
    # Unicode properties over a UTF-8 corpus
    "cp-width/noop|+codepoint-width --mode=noop --data=$D/utf8.txt"
    "cp-width/wcwidth|+codepoint-width --mode=wcwidth --data=$D/utf8.txt"
    "cp-width/simd|+codepoint-width --mode=simd --data=$D/utf8.txt"
    "cp-width/table|+codepoint-width --mode=table --data=$D/utf8.txt"
    "grapheme/noop|+grapheme-break --mode=noop --data=$D/utf8.txt"
    "grapheme/table|+grapheme-break --mode=table --data=$D/utf8.txt"
    "is-symbol/uucode|+is-symbol --mode=uucode --data=$D/utf8.txt"
    "is-symbol/table|+is-symbol --mode=table --data=$D/utf8.txt"
    # Screen clone / render-state snapshot (renderer lock holders)
    "screen-clone/noop|+screen-clone --mode=noop --data=$D/styled-4m.vt"
    "screen-clone/clone|+screen-clone --mode=clone --data=$D/styled-4m.vt"
    "screen-clone/render|+screen-clone --mode=render --loops=1 --data=$D/styled-4m.vt"
    "screen-clone/render-clean|+screen-clone --mode=render-clean --loops=1 --data=$D/styled-4m.vt"
    "screen-clone/render-partial|+screen-clone --mode=render-partial --loops=1 --data=$D/styled-4m.vt"
    "screen-clone/render-scroll|+screen-clone --mode=render-scroll --loops=1 --data=$D/styled-4m.vt"
    "screen-clone/render-output|+screen-clone --mode=render-output --loops=1 --data=$D/styled-4m.vt"
    # Page data structures
    "hyperlink-map/lookup|+hyperlink-map --mode=lookup --loops=20000"
    "hyperlink-map/churn|+hyperlink-map --mode=churn --loops=2000"
    "page-compression/compress|+page-compression --mode=compress --loops=5 --data=$D/styled.vt"
    "page-compression/store|+page-compression --mode=store --loops=5 --data=$D/styled.vt"
    "page-compression/decompress|+page-compression --mode=decompress --loops=20 --data=$D/styled.vt"
    "scrollback-compression/noop|+scrollback-compression --mode=noop --max-scrollback=200000000 --data=$D/styled.vt"
    "scrollback-compression/compress|+scrollback-compression --mode=compress --max-scrollback=200000000 --data=$D/styled.vt"
    "scrollback-compression/incremental|+scrollback-compression --mode=incremental --max-scrollback=200000000 --data=$D/styled.vt"
    "scrollback-compression/restore|+scrollback-compression --mode=restore --max-scrollback=200000000 --data=$D/styled.vt"
    # Formatter (copy / selection / screen dump)
    "formatter/noop|+terminal-formatter --mode=noop --data=$D/styled-640k.vt"
    "formatter/plain|+terminal-formatter --emit=plain --loops=200 --data=$D/styled-640k.vt"
    "formatter/vt|+terminal-formatter --emit=vt --loops=200 --data=$D/styled-640k.vt"
    "formatter/html|+terminal-formatter --emit=html --loops=200 --data=$D/styled-640k.vt"
    "formatter/terminal-roundtrip|+terminal-formatter --formatter=terminal --mode=roundtrip --loops=20 --data=$D/styled-640k.vt"
    # Resize with reflow
    "resize/noop|+terminal-resize --mode=noop --loops=1"
    "resize/cols|+terminal-resize --mode=cols --loops=1"
    "resize/cols-no-reflow|+terminal-resize --mode=cols-no-reflow --loops=1"
    "resize/rows|+terminal-resize --mode=rows --loops=1"
    "resize/both|+terminal-resize --mode=both --loops=1"
    # Snapshot encode/decode
    "snapshot/noop|+terminal-snapshot --mode=noop --data=$D/ascii-1m.vt"
    "snapshot/encode|+terminal-snapshot --mode=encode --loops=500 --data=$D/ascii-1m.vt"
    "snapshot/decode|+terminal-snapshot --mode=decode --loops=100 --data=$D/ascii-1m.vt"
    "snapshot/decode-ready|+terminal-snapshot --mode=decode-ready --loops=1000 --data=$D/ascii-1m.vt"
    # Process start + global init + arg parsing only (no --data: the step
    # returns immediately). The Zig `ghostty` exe needs the GTK apprt, so
    # this stands in for a `ghostty +version` startup comparison.
    "startup/bench-no-data|+terminal-parser"
    "snapshot/encode-kitty|+terminal-snapshot --mode=encode --loops=100 --kitty-images=8 --kitty-width=512 --kitty-height=512"
)

#---------------------------------------------------------------------------
# Run
#---------------------------------------------------------------------------

HYPERFINE=${HYPERFINE:-$(command -v hyperfine || true)}

time_cmds() { # time_cmds <json-out> <runs> <name> <cmd> [<name> <cmd>...]
    local out=$1 runs=$2; shift 2
    if [ -n "$HYPERFINE" ]; then
        local args=()
        while [ $# -gt 0 ]; do args+=(-n "$1" "$2"); shift 2; done
        "$HYPERFINE" -N --style=none --warmup "$WARMUP" --runs "$runs" \
            --export-json "$out" "${args[@]}" >/dev/null
    else
        # Fallback: same JSON shape as hyperfine's export.
        python3 -I - "$out" "$runs" "$WARMUP" "$@" <<'EOF'
import json, shlex, statistics, subprocess, sys, time
out, runs, warm = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
rest = sys.argv[4:]
results = []
for name, cmd in zip(rest[0::2], rest[1::2]):
    argv = shlex.split(cmd)
    for _ in range(warm):
        subprocess.run(argv, check=True, stdout=subprocess.DEVNULL)
    times = []
    for _ in range(runs):
        t = time.perf_counter()
        subprocess.run(argv, check=True, stdout=subprocess.DEVNULL)
        times.append(time.perf_counter() - t)
    results.append({"command": name, "median": statistics.median(times),
                    "mean": statistics.mean(times),
                    "stddev": statistics.stdev(times) if len(times) > 1 else 0,
                    "times": times})
json.dump({"results": results}, open(out, "w"))
EOF
    fi
}

if [ "$REPORT_ONLY" = 0 ]; then
    log "Running $(printf '%s\n' "${CASES[@]}" | grep -cE "${FILTER:-.}") cases (runs=$RUNS, warmup=$WARMUP, timer=${HYPERFINE:-python})"
    for c in "${CASES[@]}"; do
        id=${c%%|*}
        args=${c#*|}
        [ -n "$FILTER" ] && ! [[ $id =~ $FILTER ]] && continue
        out=$RES/$(echo "$id" | tr '/' '_').json
        cmds=(zig "$ZB/ghostty-bench $args" rust "$RB/ghostty-bench $args")
        [ "$RUST_NATIVE" = 1 ] && cmds+=(rust-native "$RNB/ghostty-bench $args")
        [ "$ZIG_BASELINE" = 1 ] && cmds+=(zig-baseline "$ZBB/ghostty-bench $args")
        printf '  %-36s' "$id" >&2
        runs=$RUNS
        [[ $id == startup/* ]] && runs=$((RUNS * 10))
        if time_cmds "$out" "$runs" "${cmds[@]}" 2>"$out.err"; then
            echo ok >&2
        else
            echo "FAILED (see $out.err)" >&2
            rm -f "$out"
        fi
    done
fi

#---------------------------------------------------------------------------
# Report
#---------------------------------------------------------------------------

python3 -I - "$RES" "$ZB" "$RB" "${CASES[@]}" <<'EOF'
import json, os, shutil, subprocess, sys, tempfile
res, zb, rb = sys.argv[1], sys.argv[2], sys.argv[3]
cases = [c.split("|", 1)[0] for c in sys.argv[4:]]

def load(cid):
    p = os.path.join(res, cid.replace("/", "_") + ".json")
    if not os.path.exists(p):
        return None
    out = {}
    for r in json.load(open(p))["results"]:
        if "summary" in r:  # hyperfine >= 2.0
            wall = r["summary"]["time_wall_clock"]
            rss = r["summary"].get("memory_peak_resident", {}).get("median")
            out[r["name"]] = {"median": wall["median"], "stddev": wall["stddev"], "rss": rss}
        else:  # hyperfine 1.x / python fallback
            out[r["command"]] = {"median": r["median"], "stddev": r["stddev"],
                                 "rss": (r.get("memory_usage_byte") or [None])[0]}
    return out

def fmt(r):
    if r is None:
        return "n/a"
    return f"{r['median']*1e3:,.1f} ± {r['stddev']*1e3:,.1f}"

def rss(r):
    return f"{r['rss']/2**20:,.0f}" if r and r.get("rss") else "n/a"

# Optional extra variants, shown when any result has them.
extras = [("rust-native", "Rust native"), ("zig-baseline", "Zig baseline")]
loaded = {cid: load(cid) for cid in cases}
extras = [(k, t) for k, t in extras
          if any(r and k in r for r in loaded.values())]

hdr = "| benchmark | Zig ms (median ± sd) | Rust ms | Rust/Zig |"
sep = "|---|---:|---:|---:|"
for k, t in extras:
    hdr += f" {t} ms | {t}/Zig |"
    sep += "---:|---:|"
    if k == "zig-baseline":
        # Same ISA as the default Rust build: the like-for-like ratio.
        hdr += " Rust/Zig baseline |"
        sep += "---:|"
hdr += " Zig / Rust peak RSS MiB |"
sep += "---:|"
print(hdr); print(sep)
for cid in cases:
    r = loaded[cid]
    if r is None or "zig" not in r or "rust" not in r:
        ncols = 2 * len(extras) + sum(k == "zig-baseline" for k, _ in extras)
        print(f"| {cid} | failed | failed | – |" + " – |" * ncols + " – |")
        continue
    z, ru = r["zig"], r["rust"]
    row = f"| {cid} | {fmt(z)} | {fmt(ru)} | {ru['median']/z['median']:.2f}× |"
    for k, _ in extras:
        n = r.get(k)
        row += f" {fmt(n)} | {n['median']/z['median']:.2f}× |" if n else " n/a | – |"
        if k == "zig-baseline":
            row += f" {ru['median']/n['median']:.2f}× |" if n else " – |"
    row += f" {rss(z)} / {rss(ru)} |"
    print(row)

print()
print("| binary | Zig bytes | Rust bytes | Zig stripped | Rust stripped |")
print("|---|---:|---:|---:|---:|")
strip = shutil.which("strip")
def sizes(p):
    if not os.path.exists(p):
        return ("n/a", "n/a")
    raw = os.path.getsize(p)
    if not strip:
        return (f"{raw:,}", "n/a")
    with tempfile.TemporaryDirectory() as d:
        q = os.path.join(d, "b")
        subprocess.run([strip, "-o", q, p], check=True)
        return (f"{raw:,}", f"{os.path.getsize(q):,}")
for b in ("ghostty-bench", "ghostty-gen"):
    zr, zs = sizes(os.path.join(zb, b))
    rr, rs = sizes(os.path.join(rb, b))
    print(f"| {b} | {zr} | {rr} | {zs} | {rs} |")
EOF
