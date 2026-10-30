# Zig Ghostty vs the Rust port: benchmarks

This compares the original Zig Ghostty with this Rust port using the
micro-benchmark CLI both sides ship (`ghostty-bench`), with the same inputs
and the same flags on the same machine. `tools/bench_compare.sh` reproduces
everything on this page: it builds both sides, generates the data, runs
hyperfine and prints the tables.

## Environment

| | |
|---|---|
| CPU | Intel Xeon Processor @ 2.10 GHz, 4 vCPU (KVM guest), AVX2 + AVX-512F/BW, 8 MiB L2, 260 MiB L3 |
| Memory / OS | 15 GB, Linux 6.18.44 (x86_64), glibc |
| Zig side | Ghostty `93967b7f9478e94dcbb42125c8a90f34feb40d75`, Zig 0.17.0 |
| Rust side | ghostty-rs `30a4945`, rustc 1.97.0 (2d8144b78 2026-07-07) |
| Timer | hyperfine 2.0.0 (`-N`, no shell) |

The machine was shared with another agent running cargo builds. Load was
about 1 for most of the runs, but the VM is noisy: most results have a
5–10 % standard deviation, and runs under 100 ms vary by up to 15 %.

## Builds

| | Zig | Rust |
|---|---|---|
| Command | `zig build -Doptimize=ReleaseFast -Demit-bench -Dapp-runtime=none` | `cargo build --release --no-default-features --bin ghostty-bench --bin ghostty-gen` |
| Target CPU | **native** (Zig's default: AVX-512 here) | **x86-64 baseline** (rustc's default) |
| SIMD | `-Dsimd=true` (default): C++ Highway and simdutf | pure Rust: `std::arch`/`memchr` where ported, scalar elsewhere |
| Codegen | single LLVM module, no safety checks | default release profile: opt-level 3, 16 codegen units, thin-local LTO, overflow checks off, bounds checks on |

On Linux the default `-Dapp-runtime=gtk` pulls the GTK app runtime into the
bench build, and that fails here (no `gtk4-layer-shell` headers and no
`blueprint-compiler`). `-Dapp-runtime=none` builds the same benchmark code
without the app runtime.

The two defaults use different instruction sets, so two extra builds separate
ISA effects from code quality:

* **Zig baseline**: `-Dcpu=baseline`. It uses the same ISA as the default Rust
  build, so **Rust/Zig baseline is the like-for-like code comparison**.
  Highway and simdutf still pick AVX2 at runtime in this build.
* **Rust native**: `RUSTFLAGS="-C target-cpu=native"`.

## Method

* The inputs are generated once by the **Zig** `ghostty-gen` and read by all
  binaries from the same files under `$WORK/data`:
  * `ascii.vt`, `ascii-lines.vt` (`--line-max=160`), `utf8.txt`, `styled.vt`,
    `kitty.apc`: 64 MiB each.
  * `osc.vt` (streaming OSC): 8 MiB. `osc.bin` (length-prefixed, parser
    style): 32 MiB.
  * `seq.txt`: `seq 1 N`, 128 MiB. This is a large `cat`-style log.
  * Small `styled` and `ascii` files for the screen-clone, formatter and
    snapshot cases.
* hyperfine runs every case with the same `ghostty-bench +<bench> <flags>`
  on each binary: 2 warmups and 8 runs. The short or noisy cases were re-run
  with 3 warmups and 20 runs, and the startup case uses 80 runs. The number
  reported is the **median wall-clock time of the whole process**, as
  Ghostty's own `src/benchmark/AGENTS.md` prescribes. It includes reading the
  data file and any setup. The `noop` modes show that fixed cost.
* There were two full passes. The Rust/Zig ratios from the two passes agree
  within about 10 %. The table below is the second pass, with the re-runs
  merged in.
* `loops` values were picked so that each run takes about 0.1 to 2 s. Some
  benchmarks scale `loops` internally. For example, `screen-clone --mode=render`
  does 50 000 × `loops` updates, and `terminal-resize` does 50 M, 2.5 M or 25
  resizes × `loops`, depending on the mode.
* The profiles come from `valgrind --tool=callgrind` (Ir counts) on 8 MiB
  inputs. `perf` is not available in this VM. Valgrind cannot run the
  AVX-512 Zig build, so the Zig profiles are from the `-Dcpu=baseline` build.

## Results

Ratios above 1 mean Rust is slower. "Zig / Rust peak RSS" is the median
maximum resident set size.

| benchmark | Zig ms (median ± sd) | Rust ms | Rust/Zig | Rust native ms | Rust native/Zig | Zig baseline ms | Zig baseline/Zig | Rust/Zig baseline | Zig / Rust peak RSS MiB |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| stream/ascii | 87.4 ± 15.0 | 144.7 ± 17.0 | 1.66× | 135.2 ± 16.5 | 1.55× | 104.0 ± 15.0 | 1.19× | 1.39× | 6 / 6 |
| stream/ascii-lines | 151.4 ± 16.2 | 222.8 ± 22.4 | 1.47× | 218.9 ± 16.5 | 1.45× | 164.2 ± 14.6 | 1.08× | 1.36× | 6 / 6 |
| stream/seq | 1,000.5 ± 32.0 | 1,143.1 ± 55.6 | 1.14× | 1,298.1 ± 48.5 | 1.30× | 1,015.1 ± 105.8 | 1.01× | 1.13× | 6 / 7 |
| stream/utf8 | 1,267.3 ± 55.7 | 1,491.0 ± 86.3 | 1.18× | 1,423.7 ± 40.3 | 1.12× | 1,207.6 ± 53.6 | 0.95× | 1.23× | 6 / 7 |
| stream/styled | 123.8 ± 13.5 | 228.7 ± 17.4 | 1.85× | 216.0 ± 26.1 | 1.75× | 182.8 ± 26.3 | 1.48× | 1.25× | 6 / 6 |
| stream/osc | 945.2 ± 102.7 | 717.2 ± 102.3 | 0.76× | 700.2 ± 30.1 | 0.74× | 1,024.6 ± 43.7 | 1.08× | 0.70× | 7 / 8 |
| stream/kitty | 66.5 ± 11.7 | 94.6 ± 14.0 | 1.42× | 91.9 ± 14.5 | 1.38× | 64.3 ± 12.2 | 0.97× | 1.47× | 5 / 6 |
| parser/ascii | 1,304.0 ± 210.6 | 806.2 ± 58.5 | 0.62× | 805.2 ± 43.5 | 0.62× | 1,090.2 ± 52.3 | 0.84× | 0.74× | 4 / 6 |
| parser/utf8 | 1,046.5 ± 233.5 | 832.5 ± 76.8 | 0.80× | 878.2 ± 57.6 | 0.84× | 899.4 ± 149.3 | 0.86× | 0.93× | 5 / 6 |
| parser/styled | 1,271.3 ± 147.6 | 797.8 ± 66.0 | 0.63× | 791.1 ± 55.1 | 0.62× | 1,038.2 ± 53.4 | 0.82× | 0.77× | 4 / 6 |
| osc-parser | 52.7 ± 6.5 | 150.8 ± 16.9 | 2.86× | 138.2 ± 10.0 | 2.62× | 50.4 ± 4.9 | 0.96× | 2.99× | 4 / 6 |
| apc-parser/kitty | 30.1 ± 2.7 | 73.1 ± 4.5 | 2.43× | 77.3 ± 11.2 | 2.57× | 36.6 ± 5.9 | 1.22× | 2.00× | 5 / 6 |
| cp-width/noop | 2.2 ± 0.6 | 3.4 ± 0.2 | 1.54× | 3.4 ± 0.1 | 1.53× | 1.9 ± 0.3 | 0.85× | 1.81× | 4 / 6 |
| cp-width/wcwidth | 443.7 ± 24.8 | 423.3 ± 13.7 | 0.95× | 415.8 ± 36.3 | 0.94× | 464.2 ± 27.6 | 1.05× | 0.91× | 4 / 6 |
| cp-width/simd | 695.8 ± 34.9 | 389.0 ± 18.9 | 0.56× | 395.2 ± 22.8 | 0.57× | 754.4 ± 23.3 | 1.08× | 0.52× | 4 / 6 |
| cp-width/table | 399.7 ± 13.2 | 362.7 ± 25.3 | 0.91× | 398.7 ± 29.1 | 1.00× | 390.3 ± 57.7 | 0.98× | 0.93× | 4 / 6 |
| grapheme/noop | 343.5 ± 21.6 | 357.0 ± 16.0 | 1.04× | 360.7 ± 14.0 | 1.05× | 331.3 ± 19.5 | 0.96× | 1.08× | 4 / 5 |
| grapheme/table | 386.2 ± 21.3 | 459.3 ± 43.1 | 1.19× | 499.9 ± 26.4 | 1.29× | 405.5 ± 23.5 | 1.05× | 1.13× | 4 / 6 |
| is-symbol/uucode | 414.9 ± 11.2 | 395.5 ± 52.1 | 0.95× | 382.8 ± 35.6 | 0.92× | 407.1 ± 30.2 | 0.98× | 0.97× | 4 / 6 |
| is-symbol/table | 398.3 ± 29.0 | 391.3 ± 19.7 | 0.98× | 395.3 ± 18.0 | 0.99× | 388.6 ± 22.6 | 0.98× | 1.01× | 4 / 6 |
| screen-clone/noop | 48.6 ± 8.4 | 35.6 ± 11.4 | 0.73× | 32.0 ± 5.6 | 0.66× | 48.6 ± 4.0 | 1.00× | 0.73× | 6 / 7 |
| screen-clone/clone | 151.8 ± 12.4 | 150.0 ± 13.9 | 0.99× | 142.0 ± 6.9 | 0.93× | 150.6 ± 6.8 | 0.99× | 1.00× | 97 / 89 |
| screen-clone/render | 335.8 ± 27.9 | 307.7 ± 22.3 | 0.92× | 302.4 ± 33.3 | 0.90× | 327.9 ± 30.4 | 0.98× | 0.94× | 7 / 7 |
| screen-clone/render-clean | 265.8 ± 25.6 | 158.7 ± 10.1 | 0.60× | 143.1 ± 17.8 | 0.54× | 263.5 ± 20.5 | 0.99× | 0.60× | 7 / 7 |
| screen-clone/render-partial | 315.5 ± 23.9 | 238.8 ± 30.4 | 0.76× | 255.7 ± 29.8 | 0.81× | 307.4 ± 14.2 | 0.97× | 0.78× | 7 / 7 |
| screen-clone/render-scroll | 747.3 ± 25.9 | 728.9 ± 40.9 | 0.98× | 756.7 ± 66.4 | 1.01× | 772.2 ± 25.8 | 1.03× | 0.94× | 73 / 73 |
| screen-clone/render-output | 348.3 ± 19.1 | 383.5 ± 30.9 | 1.10× | 351.7 ± 14.8 | 1.01× | 378.4 ± 22.7 | 1.09× | 1.01× | 7 / 7 |
| hyperlink-map/lookup | 467.5 ± 26.2 | 341.4 ± 17.6 | 0.73× | 332.9 ± 30.9 | 0.71× | 324.6 ± 39.9 | 0.69× | 1.05× | 5 / 6 |
| hyperlink-map/churn | 343.4 ± 13.5 | 344.5 ± 16.3 | 1.00× | 337.6 ± 31.3 | 0.98× | 274.6 ± 25.1 | 0.80× | 1.25× | 5 / 6 |
| page-compression/compress | 852.8 ± 47.1 | 1,136.2 ± 102.0 | 1.33× | 1,076.3 ± 52.5 | 1.26× | 836.2 ± 24.0 | 0.98× | 1.36× | 69 / 70 |
| page-compression/store | 824.2 ± 58.2 | 1,097.9 ± 90.0 | 1.33× | 1,062.8 ± 72.6 | 1.29× | 856.0 ± 58.2 | 1.04× | 1.28× | 69 / 70 |
| page-compression/decompress | 406.7 ± 33.3 | 484.9 ± 25.8 | 1.19× | 528.6 ± 38.2 | 1.30× | 428.3 ± 29.3 | 1.05× | 1.13× | 134 / 135 |
| scrollback-compression/noop | 457.8 ± 42.2 | 513.6 ± 33.1 | 1.12× | 487.9 ± 31.8 | 1.07× | 469.6 ± 20.2 | 1.03× | 1.09× | 196 / 197 |
| scrollback-compression/compress | 623.0 ± 54.0 | 699.1 ± 34.6 | 1.12× | 731.6 ± 33.6 | 1.17× | 661.4 ± 22.7 | 1.06× | 1.06× | 196 / 197 |
| scrollback-compression/incremental | 594.5 ± 22.7 | 711.2 ± 30.1 | 1.20× | 700.9 ± 39.1 | 1.18× | 663.5 ± 31.7 | 1.12× | 1.07× | 196 / 197 |
| scrollback-compression/restore | 836.1 ± 32.5 | 865.0 ± 26.7 | 1.03× | 864.4 ± 56.4 | 1.03× | 843.4 ± 42.6 | 1.01× | 1.03× | 230 / 231 |
| formatter/noop | 8.3 ± 1.9 | 14.5 ± 1.1 | 1.74× | 11.6 ± 5.7 | 1.39× | 9.9 ± 2.1 | 1.19× | 1.46× | 11 / 12 |
| formatter/plain | 755.6 ± 29.9 | 994.2 ± 94.3 | 1.32× | 1,026.6 ± 64.3 | 1.36× | 750.3 ± 44.2 | 0.99× | 1.33× | 12 / 13 |
| formatter/vt | 431.7 ± 29.4 | 799.7 ± 39.9 | 1.85× | 828.1 ± 43.1 | 1.92× | 423.0 ± 22.9 | 0.98× | 1.89× | 12 / 13 |
| formatter/html | 1,055.4 ± 56.3 | 1,371.6 ± 68.9 | 1.30× | 1,392.0 ± 39.3 | 1.32× | 1,040.4 ± 70.0 | 0.99× | 1.32× | 12 / 13 |
| formatter/terminal-roundtrip | 19.6 ± 0.8 | 27.2 ± 3.9 | 1.39× | 32.9 ± 4.5 | 1.68× | 26.7 ± 3.2 | 1.37× | 1.02× | 20 / 20 |
| resize/noop | 344.9 ± 21.8 | 363.4 ± 35.1 | 1.05× | 372.5 ± 18.6 | 1.08× | 350.0 ± 15.8 | 1.01× | 1.04× | 22 / 23 |
| resize/cols | 178.3 ± 26.0 | 209.8 ± 19.5 | 1.18× | 168.1 ± 18.6 | 0.94× | 192.8 ± 9.6 | 1.08× | 1.09× | 24 / 24 |
| resize/cols-no-reflow | 1,778.2 ± 138.2 | 1,646.7 ± 159.7 | 0.93× | 1,635.6 ± 122.4 | 0.92× | 1,784.0 ± 77.4 | 1.00× | 0.92× | 22 / 23 |
| resize/rows | 1,213.6 ± 89.3 | 1,348.7 ± 151.2 | 1.11× | 1,202.9 ± 59.5 | 0.99× | 1,320.3 ± 106.8 | 1.09× | 1.02× | 22 / 23 |
| resize/both | 197.3 ± 12.5 | 230.5 ± 21.3 | 1.17× | 185.2 ± 13.2 | 0.94× | 203.9 ± 32.3 | 1.03× | 1.13× | 24 / 24 |
| snapshot/noop | 13.4 ± 1.9 | 15.2 ± 3.9 | 1.13× | 18.9 ± 1.7 | 1.41× | 15.1 ± 1.1 | 1.13× | 1.01× | 14 / 15 |
| snapshot/encode | 439.8 ± 43.8 | 1,808.4 ± 82.8 | 4.11× | 1,751.3 ± 92.3 | 3.98× | 678.7 ± 40.5 | 1.54× | 2.66× | 14 / 15 |
| snapshot/decode | 435.3 ± 49.3 | 1,173.3 ± 49.0 | 2.70× | 1,206.5 ± 87.1 | 2.77× | 425.6 ± 31.7 | 0.98× | 2.76× | 22 / 22 |
| snapshot/decode-ready | 143.4 ± 16.1 | 469.0 ± 30.0 | 3.27× | 444.6 ± 36.4 | 3.10× | 144.3 ± 11.0 | 1.01× | 3.25× | 14 / 15 |
| snapshot/encode-kitty | 314.7 ± 19.7 | 365.8 ± 18.2 | 1.16× | 280.3 ± 30.3 | 0.89× | 521.8 ± 37.6 | 1.66× | 0.70× | 24 / 25 |
| startup/bench-no-data | 1.9 ± 0.5 | 2.7 ± 0.9 | 1.41× | 3.5 ± 0.5 | 1.85× | 1.8 ± 0.3 | 0.96× | 1.47× | 4 / 5 |

The exact flags for each case are in the `CASES` array of
`tools/bench_compare.sh`.

**Summary.** The summary covers the 44 non-`noop`, non-startup cases. The
geometric mean of Rust/Zig is **1.19×**. The geometric mean of
Rust/Zig baseline (same ISA) is **1.15×**. Rust is faster than Zig in 11
cases, within ±5 % in 7, and slower in 26. Peak memory is the same on both
sides, within about 1 MiB of fixed overhead.

### Binary sizes

| binary | Zig | Rust | Zig stripped | Rust stripped |
|---|---:|---:|---:|---:|
| ghostty-bench | 67,923,088 | 2,939,080 | 9,620,344 | 2,371,592 |
| ghostty-gen | 6,475,160 | 923,920 | 570,424 | 638,272 |

The Zig ReleaseFast bench tools ship with DWARF debug info, so the stripped
sizes are the ones to compare. The Zig `ghostty-bench` also statically links
Highway, simdutf, libc++ and every benchmark's dependencies. Cargo's release
profile already strips debug info but keeps the symbol table.

### Startup

The Zig `ghostty` executable cannot be built in this environment, because its
app runtime on Linux is GTK (see Builds). So `ghostty +version` could not be
compared. Instead, `startup/bench-no-data` runs `ghostty-bench
+terminal-parser` without `--data`. That measures process start, `global.init`
and argument parsing, and the benchmark step returns immediately. Zig takes
1.9 ms and Rust 2.7 ms. Both are close to the timer's resolution.

## Analysis

### Where the Rust port is slower

The Ir counts below come from callgrind on reduced runs: 8 MiB slices of the
corpora, or `--loops=20` for the snapshot and formatter cases. Both sides ran
the same inputs.

1. **Terminal snapshot encode and decode: 2.7–4.1×, the largest gap.**
   * `EncodedWidth::write_value` and `read_value` in
     `src/terminal/snapshot/grid.rs` (around lines 445–458) copy a slice whose
     length is only known at runtime (`out[..size].copy_from_slice(...)`).
     LLVM lowers that to a call to libc `memcpy` **for every cell**. Encoding
     makes 1 M calls per snapshot, and `memcpy` is 37 % of all Ir.
     `grid::encode` is another 51 %. In total the Rust encode takes 979 M Ir;
     the Zig baseline takes 308 M.
   * Decode has the same pattern in `grid::widen_cells` plus `memcpy`
     (34 % + 31 %).
   * Zig's `encodeNarrowInto` in `grid.zig` takes the width as a `comptime`
     parameter. It packs 4 cells per vector shuffle, and its stores have a
     fixed size.
   * The CRC adds a second, ISA-only effect. `src/crc32c.rs` selects the
     SSE4.2 path at compile time, so the default Rust build uses software
     CRC32C. Native Zig gets hardware CRC. This shows in `encode-kitty`, which checksums large image payloads:
     Rust native comes out at 0.89× and Zig baseline at 1.66×. The cause is
     inferred from how sensitive the case is to the ISA; this case was not
     profiled.
2. **OSC parser: about 2.9×.**
   * `osc::Capture::write_byte` (46 % self) and `osc::Parser::next` (40 %),
     both in `src/terminal/osc.rs`, are out-of-line calls for every byte.
   * The Zig bench calls `Parser.next` with `@call(.always_inline, ...)`, and
     its fixed writer is inlined.
   * Rust takes 418 M Ir and Zig baseline 139 M.
   * This does not hurt OSC handling inside the full stream: `stream/osc` is
     0.70–0.76×, in Rust's favor.
3. **APC (Kitty graphics) parser: 2.0–2.4×.**
   * The time goes to base64 decoding. The port decodes with the scalar code
     in `src/simd/base64.rs` and `src/simd/base64_scalar.rs`, which is a
     documented deviation. Zig calls simdutf's AVX2 decoder, which it selects
     at runtime even in the baseline build.
   * Rust takes 129 M Ir and Zig 26 M.
   * The same cost explains `stream/kitty` at about 1.45×.
4. **Formatter: `vt` 1.85×, `plain` and `html` 1.3×.**
   * In `src/terminal/formatter.rs`, `PageFormatter::format_with_state` takes
     74 % and `encode_codepoint` 21 %. `encode_codepoint` is marked
     `#[inline]` but stays out of line, so it is called once per cell.
   * Rust takes 1 454 M Ir; Zig's `writeCellRun` loop takes 765 M.
5. **VT stream printing: ascii, ascii-lines and styled at 1.25–1.4×
   like-for-like.**
   * `Terminal::print_slice_fill` in `src/terminal/terminal.rs` executes
     1.5–1.65× the instructions of Zig's `printSliceFill`: 161 M vs 98 M Ir
     for ascii, and 173 M vs 113 M for styled.
   * On top of that, the native Zig build gains another 15–30 % on these
     cases from AVX-512 code generation. `-C target-cpu=native` gains Rust
     almost nothing here.
   * Both sides zero recycled pages in `PageList::grow` the same way.
     callgrind shows a 5× gap in `memset` Ir, but that comes from how valgrind
     counts glibc's `rep stosb` path. It is not a real cost difference.
6. **LZ4 page compression: 1.3×.** The algorithm is the same, but
   `compress` in `src/terminal/compress/lz4.rs` executes 1.24× the
   instructions of `lz4.zig` (442 M vs 357 M Ir). The likely cause is bounds
   checks in the match-finding loop.
7. **Smaller gaps of 1.05–1.2×.** These are `stream/utf8`, `stream/seq`,
   `grapheme/table`, scrollback compression, resize and process startup. Most
   of them are within about 2σ of noise, or are dominated by the shared
   stream setup cost (see 5).

### Where the Rust port is faster

* **`terminal-parser`: 0.6–0.8×.** Zig's `Parser.next` returns its action
  array (`[3]?Action`) by value. compiler_rt `memcpy` is 50 % of the Zig run (722 M of
  1 445 M Ir). The Rust `Parser::next` avoids that copy (850 M Ir in total).
  In the full stream benchmarks this parser is bypassed for printable runs,
  so the advantage does not carry over.
* **`codepoint-width --mode=simd`: about 0.55×.** Zig calls the C++
  function `ghostty_simd_codepoint_width` (from `vt.cpp`) once per codepoint,
  so it cannot be inlined. The port's pure-Rust `simd::codepoint_width` inlines.
* **Render-state updates: `render-clean` 0.6×, `render-partial` 0.78×.** Also
  `screen-clone/noop` (0.73×) and the full-stream OSC path (`stream/osc`,
  0.70–0.76×).
* **`hyperlink-map/lookup`: 0.73× against native Zig.** It is 1.05× against
  Zig baseline, so this is a Zig native code-generation effect, not a Rust
  advantage.

### What the ISA comparison shows

The geometric mean is 1.19× against native Zig and 1.15× against
baseline Zig. Most of the gap is code-level, not the instruction set. The
exceptions are the stream print paths and CRC32C, which benefit from AVX-512
and SSE4.2 respectively. Building Rust with `-C target-cpu=native` barely
changes the overall picture (geometric mean 1.17×). It does help CRC-heavy and
resize paths. Where the Rust port is slower, the cause is usually a missed
inlining or specialisation in a per-byte or per-cell loop, as listed above.
Fixing those should bring most cases to parity.

## Not compared

* **`ghostty +version` and other `ghostty` executable startup.** The Zig
  executable needs the GTK app runtime (see Builds). The Rust
  `--no-default-features` `ghostty` builds fine, but it has no Zig
  counterpart here. `startup/bench-no-data` stands in for it.
* **Anything GUI or renderer-side.** This covers font shaping, Metal/OpenGL
  and the GTK app. It is out of scope for `ghostty-bench`.
* **`ghostty-gen` throughput.** Both generators exist, but only the Zig one is
  used, so that both sides read identical bytes. Generator speed is not part
  of the comparison.
* **`page-compression --mode=report`, `scrollback-compression --mode=report`,
  `terminal-snapshot --mode=report` and `terminal-formatter --mode=report`.**
  These modes print statistics. They are not timing modes.

## Reproducing

```sh
# Zig 0.17.0 from https://ziglang.org/download/index.json
tools/bench_compare.sh --zig /path/to/zig --rust-native --zig-baseline \
    --work /tmp/ghostty-bench-compare
# Re-print the table, or re-run a subset:
tools/bench_compare.sh --work /tmp/ghostty-bench-compare --report-only
tools/bench_compare.sh --work /tmp/ghostty-bench-compare --skip-build --skip-gen \
    --runs 20 --filter '^snapshot/'
```

Behind an HTTPS proxy that Zig's package fetcher cannot traverse (as in this
environment), download the URLs listed in `ghostty/build.zig.zon.txt` with
curl into a directory and pass it as `--deps-dir`. A file can also be named
`<zig-hash>.tar.gz`, or be a `<zig-hash>/` directory. `z2d` comes from a
GitHub archive URL, and this session could not reach it. A source copy whose
`zig fetch` hash matched the pin was used instead. `gobject` is a `.tar.zst`
file and needs `zstd`. It is only needed for the GTK runtime. Put
`hyperfine` on `PATH`, or point `HYPERFINE=` at it. Without it, the script
falls back to a Python timer with the same warmup and run counts.
