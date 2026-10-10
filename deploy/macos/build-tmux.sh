#!/usr/bin/env bash
# Build the tmux that ships inside lmux.app, so a fresh Mac needs no
# Homebrew. libevent and utf8proc are linked in statically; ncurses is the
# system one. The result runs on macOS 12+ and is cached by version.
#
#   deploy/macos/build-tmux.sh <out-dir>      (writes <out-dir>/tmux)
set -euo pipefail

tmux_ver=3.8
libevent_ver=2.1.12
utf8proc_ver=2.9.0

out=${1:?usage: build-tmux.sh <out-dir>}
repo=$(git -C "$(dirname "$0")/../.." rev-parse --show-toplevel)
work="$repo/target/tmux-build"
stamp="$work/stamp-$tmux_ver-$libevent_ver-$utf8proc_ver"
mkdir -p "$out"
if [ -x "$work/prefix/bin/tmux" ] && [ -f "$stamp" ]; then
  cp "$work/prefix/bin/tmux" "$out/tmux"
  exit 0
fi

rm -rf "$work" && mkdir -p "$work/src" "$work/prefix"
export MACOSX_DEPLOYMENT_TARGET=12.0
# The SDK is newer than the Macs we run on: calling anything newer than the
# deployment target must fail the build, not the launch.
export CFLAGS="-O2 -Werror=unguarded-availability-new"
jobs=$(sysctl -n hw.ncpu)

fetch() { # url sha256 file
  curl -fsSL "$1" -o "$work/src/$3"
  echo "$2  $work/src/$3" | shasum -a 256 -c - >/dev/null
  tar -xzf "$work/src/$3" -C "$work/src"
}
fetch "https://github.com/libevent/libevent/releases/download/release-$libevent_ver-stable/libevent-$libevent_ver-stable.tar.gz" \
  92e6de1be9ec176428fd2367677e61ceffc2ee1cb119035037a27d346b0403bb libevent.tar.gz
fetch "https://github.com/JuliaStrings/utf8proc/archive/refs/tags/v$utf8proc_ver.tar.gz" \
  18c1626e9fc5a2e192311e36b3010bfc698078f692888940f1fa150547abb0c1 utf8proc.tar.gz
fetch "https://github.com/tmux/tmux/releases/download/$tmux_ver/tmux-$tmux_ver.tar.gz" \
  e79c699c7e949dccd0a4a125e17b8d1261e311b16979dbc8eb34542f3966d82e tmux.tar.gz

echo "==> tmux: libevent $libevent_ver"
(cd "$work/src/libevent-$libevent_ver-stable" &&
  ac_cv_func_pipe2=no ./configure --prefix="$work/prefix" --disable-shared --enable-static \
    --disable-openssl --disable-samples --disable-libevent-regress >/dev/null &&
  make -j"$jobs" >/dev/null && make install >/dev/null)

echo "==> tmux: utf8proc $utf8proc_ver"
(cd "$work/src/utf8proc-$utf8proc_ver" &&
  make -j"$jobs" libutf8proc.a >/dev/null &&
  cp libutf8proc.a "$work/prefix/lib/" && cp utf8proc.h "$work/prefix/include/")

echo "==> tmux: tmux $tmux_ver"
(cd "$work/src/tmux-$tmux_ver" &&
  PKG_CONFIG=/usr/bin/false \
  LIBEVENT_CORE_CFLAGS="-I$work/prefix/include" LIBEVENT_CORE_LIBS="$work/prefix/lib/libevent_core.a" \
  LIBEVENT_CFLAGS="-I$work/prefix/include" LIBEVENT_LIBS="$work/prefix/lib/libevent.a" \
  LIBUTF8PROC_CFLAGS="-I$work/prefix/include" LIBUTF8PROC_LIBS="$work/prefix/lib/libutf8proc.a" \
  CPPFLAGS="-I$work/prefix/include" LDFLAGS="-L$work/prefix/lib" \
  ./configure --prefix="$work/prefix" --enable-utf8proc --disable-jemalloc >/dev/null &&
  make -j"$jobs" >/dev/null && make install >/dev/null)

# Only system libraries may remain: the app must run on a Mac without Homebrew.
if otool -L "$work/prefix/bin/tmux" | tail -n +2 | grep -v '^\s*/usr/lib/'; then
  echo "tmux links a non-system library"; exit 1
fi
touch "$stamp"
cp "$work/prefix/bin/tmux" "$out/tmux"
