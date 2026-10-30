#!/usr/bin/env bash
# Install Ghostty's runtime resources into <prefix>/share, mirroring
# `src/build/GhosttyResources.zig` (the Zig build's `zig-out/share`).
#
# usage: tools/install_resources.sh [prefix]   (default: target/dist)
#
# The macOS Xcode project bundles <repo>/target/dist/share/* into
# Ghostty.app, so run this before building the app.
#
# Env:
#   GHOSTTY_EMIT_THEMES=0  skip downloading the iTerm2 color theme pack.
#   CARGO_BUILD_ARGS       extra args for building ghostty-build-data.
set -euo pipefail
cd "$(dirname "$0")/.."
prefix="${1:-target/dist}"
share="$prefix/share"
mkdir -p "$share"

# The build-data generator runs on the host, like Zig's `b.graph.host`.
# shellcheck disable=SC2086
cargo build -q --release --no-default-features --bin ghostty-build-data ${CARGO_BUILD_ARGS:-}
gen=target/release/ghostty-build-data

# Terminfo: source, termcap, and the compiled database.
mkdir -p "$share/terminfo"
"$gen" +terminfo > "$share/terminfo/ghostty.terminfo"
if command -v infotocap >/dev/null; then
  infotocap "$share/terminfo/ghostty.terminfo" > "$share/terminfo/ghostty.termcap" 2>/dev/null
fi
if command -v tic >/dev/null; then
  tic -x -o "$share/terminfo" "$share/terminfo/ghostty.terminfo" 2>/dev/null
else
  echo "warning: tic not found; terminfo database not compiled" >&2
fi

# Shell integration (everything but the READMEs).
rm -rf "$share/ghostty/shell-integration"
mkdir -p "$share/ghostty"
cp -R src/shell-integration "$share/ghostty/shell-integration"
find "$share/ghostty/shell-integration" -name '*.md' -delete

# Themes (`iterm2_themes` in ghostty's build.zig.zon).
if [ "${GHOSTTY_EMIT_THEMES:-1}" != 0 ] && [ ! -d "$share/ghostty/themes" ]; then
  url="https://deps.files.ghostty.org/ghostty-themes-release-20261019-151001-86a776d.tgz"
  tmp=$(mktemp -d)
  if curl -fsSL "$url" -o "$tmp/themes.tgz"; then
    mkdir -p "$share/ghostty/themes"
    # Like Zig's package fetch, drop the archive's top-level directory.
    tar -xzf "$tmp/themes.tgz" -C "$share/ghostty/themes" --strip-components=1
    find "$share/ghostty/themes" -name '*.md' -delete
  else
    echo "warning: could not download themes from $url" >&2
  fi
  rm -rf "$tmp"
fi

# Shell completions.
mkdir -p "$share/fish/vendor_completions.d" "$share/zsh/site-functions" \
  "$share/bash-completion/completions"
"$gen" +fish > "$share/fish/vendor_completions.d/ghostty.fish"
"$gen" +zsh > "$share/zsh/site-functions/_ghostty"
"$gen" +bash > "$share/bash-completion/completions/ghostty.bash"

# Vim and Neovim plugin.
for dir in "$share/vim/vimfiles" "$share/nvim/site"; do
  mkdir -p "$dir"/{syntax,ftdetect,ftplugin,compiler}
  "$gen" +vim-syntax > "$dir/syntax/ghostty.vim"
  "$gen" +vim-ftdetect > "$dir/ftdetect/ghostty.vim"
  "$gen" +vim-ftplugin > "$dir/ftplugin/ghostty.vim"
  "$gen" +vim-compiler > "$dir/compiler/ghostty.vim"
done

# Sublime syntax for bat.
mkdir -p "$share/bat/syntaxes"
"$gen" +sublime > "$share/bat/syntaxes/ghostty.sublime-syntax"

# Translations (`src/build/GhosttyI18n.zig`): po/<locale>.po ->
# share/locale/<locale>/LC_MESSAGES/com.mitchellh.ghostty.mo.
mkdir -p "$share/locale"
if command -v msgfmt >/dev/null; then
  for po in po/*.po; do
    locale=$(basename "$po" .po)
    mkdir -p "$share/locale/$locale/LC_MESSAGES"
    msgfmt -o "$share/locale/$locale/LC_MESSAGES/com.mitchellh.ghostty.mo" "$po"
  done
else
  echo "warning: msgfmt not found; translations not compiled" >&2
fi

# Man pages are generated from the docs by the Zig build's mdgen + pandoc,
# which has no counterpart here yet; the Xcode project still expects the
# directory to exist.
mkdir -p "$share/man"

echo "resources installed to $share"
