#!/usr/bin/env bash
# 本地构建入口。用法：./build.sh check|test|build
#
# 为什么不用 vcvars64.bat、也不在 cmd 里直接调 cargo，见 AGENTS.md「环境陷阱」。
set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# 取目录下最新的一项：版本号随 Visual Studio / Windows SDK 更新变化，写死会失效。
pick_latest() {
  ls -d "$1"/*/ 2>/dev/null | sed 's:/*$::' | sort -V | tail -1 || true
}

MSVC_TOOLS=""
for vs_base in \
  "/c/Program Files (x86)/Microsoft Visual Studio" \
  "/c/Program Files/Microsoft Visual Studio"; do
  [ -d "$vs_base" ] || continue
  MSVC_TOOLS="$(ls -d "$vs_base"/*/*/VC/Tools/MSVC/*/ 2>/dev/null | sed 's:/*$::' | sort -V | tail -1 || true)"
  [ -n "$MSVC_TOOLS" ] && break
done
if [ -z "$MSVC_TOOLS" ]; then
  echo "[error] 找不到 MSVC 工具链：需要 Visual Studio Build Tools（含 C++ 生成工具）" >&2
  exit 1
fi

SDK_BASE="/c/Program Files (x86)/Windows Kits/10"
SDK_INC="$(pick_latest "$SDK_BASE/Include")"
if [ -z "$SDK_INC" ]; then
  echo "[error] 找不到 Windows SDK（$SDK_BASE/Include 为空）" >&2
  exit 1
fi
SDKVER="$(basename "$SDK_INC")"

MSVC_WIN="$(cygpath -w "$MSVC_TOOLS")"
SDK_WIN="$(cygpath -w "$SDK_BASE")"

export INCLUDE="$MSVC_WIN\\include;$SDK_WIN\\Include\\$SDKVER\\ucrt;$SDK_WIN\\Include\\$SDKVER\\shared;$SDK_WIN\\Include\\$SDKVER\\um;$SDK_WIN\\Include\\$SDKVER\\winrt"
export LIB="$MSVC_WIN\\lib\\x64;$SDK_WIN\\Lib\\$SDKVER\\ucrt\\x64;$SDK_WIN\\Lib\\$SDKVER\\um\\x64"
export PATH="$(cygpath -u "$MSVC_TOOLS")/bin/Hostx64/x64:$HOME/.cargo/bin:$PATH"

cd "$REPO_DIR"

case "${1:-}" in
  check)
    cargo check -p wb-switch-core --all-targets
    ;;
  test)
    cargo test -p wb-switch-core --lib
    ;;
  build)
    npm run build
    cargo build --release --manifest-path src-tauri/Cargo.toml
    echo "[ok] $REPO_DIR/target/release/wb-switch-rust.exe"
    ;;
  *)
    echo "usage: build.sh check|test|build" >&2
    exit 1
    ;;
esac
