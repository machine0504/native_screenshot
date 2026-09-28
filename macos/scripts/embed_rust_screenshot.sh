#!/usr/bin/env bash
# Xcode Run Script / DMG 打包：构建 rust_screenshot 并嵌入 App Bundle。
# helper 作为宿主 bundle 的内部可执行文件（Contents/MacOS/rust_screenshot），与宿主
# 共享同一 bundle 身份与“负责进程(responsible process)”；屏幕录制(TCC)权限随宿主，
# 不会在系统“隐私与安全性 > 屏幕录制”里单列出第二个条目。
set -eo pipefail

# 用脚本自身位置定位插件内的 rust 源码：本脚本在插件 macos/scripts/ 下，rust 源码在
# 插件根（../../rust_screenshot），与宿主 App 的 SRCROOT 取值无关，更健壮。
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUST_DIR="${SCRIPT_DIR}/../../rust_screenshot"
# 允许外部传入 App 路径（build_dmg.sh）；否则使用 Xcode 构建产物。
APP="${1:-${BUILT_PRODUCTS_DIR:-}/${WRAPPER_NAME:-}}"
# helper 直接放在宿主 bundle 的 Contents/MacOS 下，与宿主主程序同处一个 bundle。
TARGET_BIN="${APP}/Contents/MacOS/rust_screenshot"
OLD_HELPER_APP="${APP}/Contents/Helpers/rust_screenshot.app"
HOST_PLIST="${APP}/Contents/Info.plist"
ENTITLEMENTS="${SRCROOT:-}/Runner/ScreenshotHelper.entitlements"

if [[ ! -d "$APP" ]]; then
  echo "warning: [Screenshot] App bundle 不存在: $APP"
  exit 0
fi

# Xcode 的 Run Script 阶段使用受限 PATH（不读 ~/.zshrc / ~/.bash_profile），rustup 装在
# ~/.cargo/bin 的 cargo 因此“找不到”。这里显式补上常见安装路径，并 source rustup 的
# 环境脚本（兼容自定义 CARGO_HOME），让 Xcode 直接打包也能定位 cargo。
export PATH="${HOME}/.cargo/bin:/usr/local/bin:/opt/homebrew/bin:${PATH}"
[[ -n "${CARGO_HOME:-}" && -f "${CARGO_HOME}/env" ]] && . "${CARGO_HOME}/env"
[[ -f "${HOME}/.cargo/env" ]] && . "${HOME}/.cargo/env"

if ! command -v cargo &>/dev/null; then
  echo "error: [Screenshot] 未找到 cargo，请先安装 Rust 工具链 (https://rustup.rs)"
  exit 1
fi

HOST_BUNDLE_ID="${HOST_BUNDLE_ID:-com.example.app}"
HOST_DISPLAY_NAME="${HOST_DISPLAY_NAME:-App}"
SCREEN_CAPTURE_DESC="${SCREEN_CAPTURE_DESC:-This app needs screen recording permission for screenshots you start yourself. It does not record in the background.}"

if [[ -f "$HOST_PLIST" ]]; then
  HOST_BUNDLE_ID=$(/usr/libexec/PlistBuddy -c "Print :CFBundleIdentifier" "$HOST_PLIST" 2>/dev/null || echo "$HOST_BUNDLE_ID")
  HOST_DISPLAY_NAME=$(/usr/libexec/PlistBuddy -c "Print :CFBundleDisplayName" "$HOST_PLIST" 2>/dev/null || echo "$HOST_DISPLAY_NAME")
  if /usr/libexec/PlistBuddy -c "Print :NSScreenCaptureUsageDescription" "$HOST_PLIST" &>/dev/null; then
    SCREEN_CAPTURE_DESC=$(/usr/libexec/PlistBuddy -c "Print :NSScreenCaptureUsageDescription" "$HOST_PLIST")
  fi
fi

# helper 必须是 universal（x86_64 + arm64）。宿主 App 是通用包，可在 Intel 与 Apple
# Silicon 上运行；若 helper 只编了构建机的那一拱（在 Apple Silicon 上默认只出 arm64），
# 到 Intel Mac 上就会以子进程启动失败并报 "Bad CPU type in executable"。故这里两拱都编、
# 再 lipo 合并成单个通用二进制嵌入。
HELPER_TARGETS=(x86_64-apple-darwin aarch64-apple-darwin)

# best-effort 确保两个 target 已安装（离线/无 rustup 时忽略，靠下方存在性判断兜底）。
if command -v rustup &>/dev/null; then
  for t in "${HELPER_TARGETS[@]}"; do
    rustup target add "$t" >/dev/null 2>&1 || true
  done
fi

echo "[Screenshot] cargo build --release (universal: ${HELPER_TARGETS[*]}) ..."
SLICES=()
for t in "${HELPER_TARGETS[@]}"; do
  (cd "$RUST_DIR" && cargo build --release --target "$t") || {
    echo "warning: [Screenshot] 目标 $t 构建失败，将跳过该架构"
    continue
  }
  slice="${RUST_DIR}/target/${t}/release/rust_screenshot"
  if [[ -f "$slice" ]]; then
    SLICES+=("$slice")
  else
    echo "warning: [Screenshot] 目标 $t 未产出二进制: $slice"
  fi
done

if [[ ${#SLICES[@]} -eq 0 ]]; then
  echo "error: [Screenshot] 没有任何架构构建成功，无法嵌入 helper"
  exit 1
fi

# lipo 合并为通用二进制（仅一拱时也会产出等价的单架构文件，不致失败）。
lipo -create "${SLICES[@]}" -output "$TARGET_BIN"
chmod +x "$TARGET_BIN"
echo "[Screenshot] helper 架构: $(lipo -archs "$TARGET_BIN" 2>/dev/null || echo unknown)"
if [[ ${#SLICES[@]} -lt ${#HELPER_TARGETS[@]} ]]; then
  echo "warning: [Screenshot] helper 非全架构（缺架构的 Mac 上会报 Bad CPU type），请在联网/装齐 rustup target 的机器上重打"
fi

_sign() {
  local target="$1"
  local extra_args=()
  local ent_file=""
  if [[ -f "$ENTITLEMENTS" ]]; then
    # codesign 要求 entitlements 为 Unix(LF) 换行；CRLF 会报 AMFIUnserializeXML syntax error。
    ent_file="$(mktemp)"
    tr -d '\r' < "$ENTITLEMENTS" > "$ent_file"
    extra_args+=(--entitlements "$ent_file" --options runtime)
  fi
  if [[ -n "${EXPANDED_CODE_SIGN_IDENTITY:-}" && "${EXPANDED_CODE_SIGN_IDENTITY}" != "-" ]]; then
    codesign --force --sign "${EXPANDED_CODE_SIGN_IDENTITY}" --timestamp=none "${extra_args[@]}" "$target" \
      || codesign --force --sign "${EXPANDED_CODE_SIGN_IDENTITY}" "${extra_args[@]}" "$target"
  else
    codesign --force --sign - "${extra_args[@]}" "$target" 2>/dev/null || true
  fi
  [[ -n "$ent_file" && -f "$ent_file" ]] && rm -f "$ent_file"
}

# 仅签 helper 可执行文件本身（不再写独立 Info.plist / 不再打成 .app）。helper 的 TCC
# 归属取决于“被宿主以子进程方式启动”，与它自身的签名身份无关。
_sign "$TARGET_BIN"

# 清理旧布局：此前 helper 被打成独立 .app（Contents/Helpers/rust_screenshot.app），
# 其独立 bundle 身份会让系统当作独立 App 单独申请屏幕录制权限（即系统把它当成另一个 App 的
# 来源），这里一并移除。
if [[ -d "$OLD_HELPER_APP" ]]; then
  rm -rf "$OLD_HELPER_APP"
  echo "[Screenshot] 已移除旧版独立 Helper App: $OLD_HELPER_APP"
fi

echo "[Screenshot] 已嵌入: $TARGET_BIN ($(wc -c < "$TARGET_BIN" | tr -d ' ') bytes)"
echo "[Screenshot] Helper 复用宿主 bundle 身份: ${HOST_BUNDLE_ID}"
