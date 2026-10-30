# ghostty-rs

A file-for-file port of the [Ghostty](https://github.com/ghostty-org/ghostty)
terminal emulator from Zig to Rust (reference revision `93967b7`).

![The Rust port running as a GTK terminal](docs/screenshot-gtk.png)

Every `ghostty/src/**/*.zig` (and `ghostty/pkg/**/*.zig`) file has a Rust
counterpart at the mirrored snake_case path, e.g. `src/terminal/PageList.zig`
→ `src/terminal/page_list.rs`, with its tests ported alongside. See
[PORTING.md](PORTING.md) for the conventions and `PORTING_STATUS.tsv` for the
full file mapping.

## Building and running (Linux)

System libraries: GTK 4, libadwaita, FreeType, HarfBuzz, fontconfig,
Oniguruma, EGL/GL. On Debian/Ubuntu:

```sh
sudo apt install libgtk-4-dev libadwaita-1-dev libfreetype-dev libharfbuzz-dev \
  libfontconfig-dev libonig-dev libegl-dev
cargo run --release              # the GTK terminal
cargo run --release -- +version  # CLI actions work as in Ghostty
cargo test --lib                 # the ported test suite
```

`cargo build --no-default-features` builds without the GTK runtime.
`--features inspector` enables the Dear ImGui terminal inspector.
`ghostty-bench` and `ghostty-gen` are the benchmark and synthetic-data tools;
`ghostty-build-data` generates the terminfo, completion and syntax files.

`tools/install_resources.sh [prefix]` installs the runtime resources (terminfo,
shell integration, themes, completions, vim/bat syntax, translations) into
`<prefix>/share` (default `target/dist/share`), like the Zig build's
`zig-out/share`.

## The macOS app

`macos/` is Ghostty's Swift/AppKit app, unchanged except that it bundles
resources from `target/dist/share`. It links `GhosttyKit.xcframework`, the
libghostty C API (`include/ghostty.h`), which the Rust port provides through
`src/apprt/embedded.rs` and `src/main_c.rs`. On macOS with Xcode:

```sh
brew install pkg-config freetype harfbuzz fontconfig oniguruma
tools/install_resources.sh               # target/dist/share
tools/build_xcframework.sh native        # or: universal (arm64 + x86_64)
macos/build.nu --configuration Debug     # -> macos/build/Debug/Ghostty.app
```

`tools/build_xcframework.sh` replaces `zig build -Demit-macos-app=false`.

## Progress

```sh
python3 tools/status.py      # summary
python3 tools/status.py -v   # per file
```

Each Rust file's header says `STATUS: ported`, `partial — ...`, or
`not yet ported`; every Zig file is now ported. macOS code (Metal, CoreText,
the embedded apprt) is ported and type-checks with
`cargo check --target x86_64-apple-darwin`, but has not been run, and the
Swift app has not been built against the Rust library yet.

## Working with the reference source

```sh
git clone https://github.com/ghostty-org/ghostty.git ghostty
git -C ghostty checkout 93967b7f9478e94dcbb42125c8a90f34feb40d75
```
