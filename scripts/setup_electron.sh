#!/bin/bash
# 离线落位 Electron 二进制。
#
# 为什么需要它：`npm install electron` 只装 JS 包，真正的二进制由 postinstall
# 从 GitHub Releases 下载——在本沙箱里这一步会被拦（表现为 `npx electron` 永远停在
# "Downloading Electron binary..."，最后超时）。
#
# 但直接 curl 那个 Release 是通的，所以这里手工下载 + 解压 + 写 path.txt，
# 把 electron 包该有的布局补齐（与 postinstall 的结果一致）：
#   node_modules/electron/dist/Electron.app/...
#   node_modules/electron/path.txt   ← 二进制相对路径
#
# 用法：scripts/setup_electron.sh [版本，默认取 client 依赖里的]

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CLIENT="$ROOT/client"
CACHE="$HOME/.cache/b-artifact-tools/electron"

VERSION="${1:-}"
if [ -z "$VERSION" ]; then
  VERSION="$("$CLIENT/node_modules/.bin/node" -e '
    const fs=require("fs");
    const p=JSON.parse(fs.readFileSync(process.argv[1],"utf8"));
    console.log(String(p.devDependencies.electron||"^44.0.0").replace(/[^0-9.]/g,""));
  ' "$CLIENT/package.json" 2>/dev/null || true)"
fi
[ -n "$VERSION" ] || VERSION="44.3.0"

case "$(uname -s)" in
  Darwin) OS="darwin" ;;
  Linux) OS="linux" ;;
  *) echo "不支持的系统：$(uname -s)"; exit 1 ;;
esac
case "$(uname -m)" in
  arm64 | aarch64) ARCH="arm64" ;;
  *) ARCH="x64" ;;
esac

ZIP="electron-v${VERSION}-${OS}-${ARCH}.zip"
DEST="$CLIENT/node_modules/electron"

if [ ! -d "$DEST" ]; then
  echo "先装 electron 包：cd client && npm install"
  exit 1
fi
if [ -x "$DEST/dist/Electron.app/Contents/MacOS/Electron" ] || [ -x "$DEST/dist/electron" ]; then
  echo "Electron 二进制已就位，跳过"
  exit 0
fi

mkdir -p "$CACHE"
if [ ! -f "$CACHE/$ZIP" ]; then
  URL="https://github.com/electron/electron/releases/download/v${VERSION}/${ZIP}"
  echo "下载 $URL"
  curl -fsSL --retry 2 -o "$CACHE/$ZIP" "$URL"
fi

mkdir -p "$DEST/dist"
unzip -oq "$CACHE/$ZIP" -d "$DEST/dist"

if [ "$OS" = "darwin" ]; then
  xattr -dr com.apple.quarantine "$DEST/dist" 2>/dev/null || true
  printf 'Electron.app/Contents/MacOS/Electron' > "$DEST/path.txt"
else
  printf 'electron' > "$DEST/path.txt"
fi

echo "Electron $VERSION 已落位到 $DEST/dist"
