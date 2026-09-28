# native_screenshot

桌面截图插件，交互接近 Snipaste：冻结画面、框选、放大镜、取色、标注，确认后得到 PNG，并写入系统剪贴板。

截图界面由 Rust 程序 `rust_screenshot` 完成（winit + tiny-skia）。Flutter 只负责启动这个程序并读回图片。支持 Windows、macOS、Linux。

## 环境

- Flutter 3.10+
- Rust 工具链，`cargo` 在 PATH 中：<https://rustup.rs>
- macOS 还需要给宿主 App 打开「屏幕录制」权限

## 接入

```yaml
dependencies:
  native_screenshot:
    git:
      url: https://github.com/machine0504/native_screenshot.git
  window_manager: ^0.4.0
```

```dart
import 'package:native_screenshot/native_screenshot.dart';

final png = await NativeScreenshot.captureRegion(
  minimizeWindow: true,
  onPermissionDenied: () {
    // 仅 macOS：引导用户到系统设置打开屏幕录制
  },
  onError: (reason) {
    // 启动失败、helper 没打进包等
  },
);
```

`minimizeWindow: true` 时先把本窗口藏起来，截到的是桌面而不是本应用。取消时返回 `null`。

应用启动后可以预热一次：

```dart
await NativeScreenshot.warmUp();
```

## 打包

Windows 和 Linux：插件的 CMake 会在构建时执行 `cargo build --release`，并把 helper 拷到程序旁边。

macOS：在 Xcode 的 Run Script 里执行：

```bash
bash "$SRCROOT/../.symlinks 或插件路径/macos/scripts/embed_rust_screenshot.sh"
```

脚本会读取宿主 `Info.plist` 的 Bundle ID 和应用名。读不到时使用 `com.example.app`。也可以在脚本前设置环境变量 `HOST_BUNDLE_ID`、`HOST_DISPLAY_NAME`。

更细的行为说明见 [SPEC.md](SPEC.md)。

## 示例

```bash
cd example
flutter run -d macos
```
