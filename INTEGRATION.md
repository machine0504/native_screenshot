# native_screenshot 集成与封装指南

> 本文档说明：截图功能已**整体封装进 `native_screenshot` 插件**，宿主 App（`desktop/`）
> 几乎零维护。包含目录结构、宿主集成步骤（按平台）、权限模型与验证清单。

## 1. 概述

截图功能现在是一个自包含的 Flutter plugin —— `package/native_screenshot`。它打包了：

- 跨平台 Rust 截图程序 `rust_screenshot`（winit + tiny-skia，负责冻结快照 / 框选 / 放大镜 / 取色 / 标注 / 输出）；
- 三端的构建与随包分发集成（macOS 脚本、Linux/Windows 的 CMakeLists）；
- Dart 高层 API（权限检查 + 窗口最小化/恢复 + 启动 helper 取回 PNG）。

宿主 App 要做的只剩：**依赖插件 + 调用一个 API**，外加 **macOS 上一次性的权限/脚本配置**（系统机制决定，无法由插件代劳）。

## 2. 目录结构

```
package/native_screenshot/
├── lib/
│   ├── native_screenshot.dart            # 库入口（导出）
│   └── src/
│       ├── native_screenshot_api.dart    # NativeScreenshot.captureRegion()（对外高层 API）
│       └── screenshot_controller.dart    # 启动 helper 子进程、解析结果、定位 helper
├── rust_screenshot/                      # ← Rust helper 源码（含 .cargo/config.toml 国内镜像）
│   ├── Cargo.toml / Cargo.lock / src/...
├── macos/
│   └── scripts/embed_rust_screenshot.sh  # macOS：cargo 构建 + 嵌入 Contents/MacOS + 签名
├── linux/CMakeLists.txt                  # Linux：cargo 构建 + 拷贝 helper 到 bundle
├── windows/CMakeLists.txt                # Windows：cargo 构建 + 拷贝 helper.exe 到 App 同目录
├── pubspec.yaml                          # Flutter plugin 声明（linux/windows ffiPlugin）
├── SPEC.md                               # 截图交互行为规范（三端一致性契约）
└── INTEGRATION.md                        # 本文档
```

## 3. 宿主 App 集成

### 3.1 通用（Dart 侧）

1) `pubspec.yaml` 依赖（path 依赖，注意保持 `package/` 与 `desktop/` 相对位置）：

```yaml
dependencies:
  native_screenshot:
    path: ../package/native_screenshot
```

2) 调用（一个方法搞定，权限/窗口/调用都在插件内）：

```dart
import 'package:native_screenshot/native_screenshot.dart';

// 最小化本窗口后截图（本应用不出现在截图里）
final png = await NativeScreenshot.captureRegion(
  minimizeWindow: true,
  onPermissionDenied: () => showYourHint(), // 仅 macOS 无权限时回调，宿主用自己的 UI 提示
);

// 不隐藏窗口（“截当前界面”）
final png2 = await NativeScreenshot.captureRegion(minimizeWindow: false);
```

> 本项目里 `desktop/lib/common/desktop_snip/desktop_snip_flow.dart` 已是对上面 API 的薄封装
> （`runDesktopSnip` / `runDesktopSnipKeepWindow`），只负责把“无权限提示”接到应用的消息组件
> `$message`。调用方（tab 栏按钮）签名不变。

### 3.2 Linux / Windows —— 零配置 ✅

插件的 `linux/CMakeLists.txt` 与 `windows/CMakeLists.txt` 会被宿主 App 的 Flutter 构建
**自动接管**（`flutter/generated_plugins.cmake` 负责 `add_subdirectory`）：构建时执行
`cargo build --release` 并把 helper 拷到 App 同目录。宿主 App 无需任何配置。

唯一要求：**构建机安装 Rust 工具链**（`cargo` 在 PATH 中）。

### 3.3 macOS —— 一次性配置 2 项 ⚠️

CocoaPods/插件机制无法把“可执行文件”塞进 `App.app/Contents/MacOS/`（这是 helper 复用宿主
身份、只占一个屏幕录制权限的关键），因此 macOS 上宿主 App 需保留以下 **一次性** 配置：

**(A) 屏幕录制权限声明**（Runner 工程，属 app 主体，插件无法注入）：

- `macos/Runner/Info.plist` → `NSScreenCaptureUsageDescription`（用途描述）。
- `macos/Runner/Release.entitlements`、`DebugProfile.entitlements`、`RunnerDebug.entitlements`
  → `com.apple.security.device.screen-capture`（及 `com.apple.screencapture.interactive` 的
  mach-lookup/register 临时例外）。

**(B) 一个 Run Script 构建阶段**（Xcode → Runner target → Build Phases）：

```sh
bash "${SRCROOT}/../../package/native_screenshot/macos/scripts/embed_rust_screenshot.sh"
```

- 放在 Build Phases 末尾；取消勾选 “Based on dependency analysis”（每次构建都执行）。
- 该脚本会：`cargo build --release` → 把 helper 拷到 `App.app/Contents/MacOS/rust_screenshot`
  → 用宿主签名身份做签名 → 清理旧版独立 Helper.app。
- DMG 打包脚本 `desktop/macos安装程序/scripts/build_dmg.sh` 已自动调用同一脚本，无需另配。

> 以上两项配一次即可，插件后续升级都不用再动。

## 4. 权限模型（macOS 重点）

- helper 由宿主以**子进程**方式启动，且二进制位于宿主 `Contents/MacOS/` 内、复用宿主
  bundle 身份。因此其屏幕录制权限的“负责进程（responsible process）”归属到宿主，
  系统「隐私与安全性 → 屏幕录制」里**只出现宿主一个条目**（不会出现两个同名 App）。
- helper **不**自行调用 `CGRequestScreenCaptureAccess()`；权限由宿主持有。
- `NativeScreenshot.captureRegion()` 在截图前会自动检查/申请宿主的屏幕录制权限
  （经 `screen_capturer`）。

## 5. 构建机要求

- Rust 工具链（`rustup` / `cargo`）。
- 插件内 `rust_screenshot/.cargo/config.toml` 已配 rsproxy 国内镜像（仅作用于该 crate，
  不影响全局）。

## 6. 验证清单（迁移/升级后逐项确认）

- [ ] `cargo build --release`（在 `package/native_screenshot/rust_screenshot/`）通过。
- [ ] `flutter pub get` 后 `flutter analyze` 无错（插件 + 宿主）。
- [ ] **macOS**：`flutter run` / `build_dmg.sh`；截图正常；系统「屏幕录制」只有**一个**宿主条目。
- [ ] **Windows**：`flutter run` / `flutter build windows`；`rust_screenshot.exe` 在 App 同目录；截图正常。
- [ ] **Linux**：`flutter run` / `flutter build linux`；helper 在 bundle 根（与可执行同级）；截图正常。

## 7. 与旧布局的差异（迁移备忘）

| 项 | 旧（散在 app） | 新（封装进插件） |
| --- | --- | --- |
| Rust 源码 | `desktop/rust_screenshot/` | `package/native_screenshot/rust_screenshot/` |
| macOS 嵌入脚本 | `desktop/macos/scripts/embed_rust_screenshot.sh` | `package/native_screenshot/macos/scripts/embed_rust_screenshot.sh` |
| macOS 触发 | app pbxproj Run Script（硬编码脚本） | app pbxproj Run Script（指向插件脚本，一次性） |
| Linux 构建 | `desktop/linux/CMakeLists.txt` 的 rust 段 | `package/native_screenshot/linux/CMakeLists.txt`（自动） |
| Windows 构建 | 无（开发期靠 Dart 查找 target） | `package/native_screenshot/windows/CMakeLists.txt`（自动，补齐打包缺口） |
| Dart 业务流程 | `desktop/.../desktop_snip_flow.dart`（全逻辑） | 插件 `NativeScreenshot`；app 仅薄转发 |
