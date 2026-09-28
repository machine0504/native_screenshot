import 'dart:async';
import 'dart:typed_data';

import 'package:flutter/foundation.dart'
    show VoidCallback, debugPrint, defaultTargetPlatform, kIsWeb;
import 'package:flutter/widgets.dart' show TargetPlatform;
import 'package:screen_capturer/screen_capturer.dart';
import 'package:window_manager/window_manager.dart';

import 'screenshot_controller.dart';

/// 截图对外高层 API。
///
/// 整套 Snipaste 式交互（冻结快照 + 框选 + 放大镜 + 取色 + 标注）由随包分发的 Rust
/// helper 完成；本类把「屏幕录制权限检查（macOS）+ 截图时最小化/恢复宿主窗口 + 调用
/// helper 取回 PNG」全部封装在插件内，宿主 App 只需调用一个方法、按需接一个提示回调。
class NativeScreenshot {
  NativeScreenshot._();

  /// 预热常驻 helper（App 启动后调用一次即可；失败不影响后续截图）。
  static Future<void> warmUp() =>
      ScreenshotController.instance.warmUp();

  /// 区域截图：进入全屏遮罩，框选 + 标注，确认后返回选区 PNG（同时已写入系统剪贴板）；
  /// 用户取消、失败或无权限均返回 `null`。
  ///
  /// - [minimizeWindow]：`true`（默认）先最小化宿主窗口，使本应用不出现在截图里，截图
  ///   结束后自动恢复窗口；`false` 则保留窗口（“截当前界面”场景）。
  /// - [onPermissionDenied]：仅 macOS，当缺少“屏幕录制”权限、且弹出系统授权后用户仍未
  ///   授权时回调（供宿主用自己的 UI 提示用户去系统设置授权）。
  /// - [onError]：当截图因**基础设施类错误**失败时回调一个简短原因（如 helper 未打包、
  ///   启动失败、捕获/遮罩出错等），供宿主弹错误提示。**用户主动取消不会触发**，返回值仍为
  ///   `null`。统信/麒麟等 Linux 上若“点击无反应”，多半是这里能拿到具体原因。
  static Future<Uint8List?> captureRegion({
    bool minimizeWindow = true,
    VoidCallback? onPermissionDenied,
    void Function(String reason)? onError,
  }) async {
    if (!await _ensureScreenCapturePermission()) {
      onPermissionDenied?.call();
      return null;
    }
    Uint8List? bytes;
    if (!minimizeWindow) {
      try {
        bytes = await _captureOverlay();
      } catch (e) {
        debugPrint('[native_screenshot] capture failed: $e');
        bytes = null;
      }
    } else {
      bytes = await _captureWithMinimize();
    }
    // 仅在「真正失败」（而非用户取消）时回调；失败原因由控制器记录。
    if ((bytes == null || bytes.isEmpty) && onError != null) {
      final reason = ScreenshotController.instance.lastFailureReason;
      if (reason != null && reason.isNotEmpty) {
        onError(reason);
      }
    }
    return bytes;
  }

  /// 一次区域截图：调用子进程 helper（[ScreenshotController]）显示遮罩并取回 PNG。
  static Future<Uint8List?> _captureOverlay() {
    return ScreenshotController.instance.captureRegionOnDesktopOverlay();
  }

  /// 先最小化主窗口再截图，结束后恢复窗口布局。macOS 用 `minimize()`（不用 `hide()`：
  /// `hide()` 后主窗口无法从 Dock 恢复，helper 异常时像“闪退”）。
  static Future<Uint8List?> _captureWithMinimize() async {
    final wasMaximized = await windowManager.isMaximized();
    final wasAlwaysOnTop = await windowManager.isAlwaysOnTop();
    final savedBounds = wasMaximized ? null : await windowManager.getBounds();
    final isMacOS = !kIsWeb && defaultTargetPlatform == TargetPlatform.macOS;
    // Windows：用 hide()/show() 取代 minimize()/restore()。minimize/restore 会触发
    // 系统「最小化/还原」缩放动画（宿主窗口缩到任务栏又弹回来，即用户看到的“放大缩小”），
    // 而 hide()=SW_HIDE / show()=SW_SHOW 是瞬时、无动画的，且窗口同样不会进入截图。
    // macOS/Linux 保持 minimize（macOS 上 hide() 会导致无法从 Dock 恢复）。
    final isWindows = !kIsWeb && defaultTargetPlatform == TargetPlatform.windows;

    Uint8List? bytes;
    try {
      if (isWindows) {
        await windowManager.hide();
      } else {
        await windowManager.minimize();
      }
      await Future.delayed(Duration(milliseconds: isMacOS ? 400 : 200));

      bytes = await _captureOverlay().timeout(const Duration(minutes: 30));
    } on TimeoutException {
      ScreenshotController.instance.cancelActiveCapture();
      bytes = null;
    } catch (e, st) {
      debugPrint('[native_screenshot] capture failed: $e\n$st');
      bytes = null;
    } finally {
      ScreenshotController.instance.cancelActiveCapture();
      try {
        if (isWindows) {
          await windowManager.show();
        } else {
          await windowManager.restore();
          await windowManager.show();
        }
        await windowManager.focus();
        await windowManager.setAlwaysOnTop(wasAlwaysOnTop);
        if (wasMaximized) {
          await windowManager.maximize();
        } else if (savedBounds != null) {
          await windowManager.setBounds(savedBounds);
        }
        await windowManager.focus();
      } catch (e, st) {
        debugPrint('[native_screenshot] restore window failed: $e\n$st');
      }
    }
    return bytes;
  }

  /// macOS：截图前确保宿主 App 已获得“屏幕录制”权限。helper 以宿主子进程方式运行、
  /// 继承宿主授权，故权限必须由宿主持有。无权限时弹系统授权请求，返回是否已授权；
  /// 非 macOS 无需该权限，直接放行。
  static Future<bool> _ensureScreenCapturePermission() async {
    final isMacOS = !kIsWeb && defaultTargetPlatform == TargetPlatform.macOS;
    if (!isMacOS) return true;
    try {
      if (await ScreenCapturer.instance.isAccessAllowed()) return true;
      await ScreenCapturer.instance.requestAccess();
      return await ScreenCapturer.instance.isAccessAllowed();
    } catch (e) {
      debugPrint('[native_screenshot] 屏幕录制权限检查失败: $e');
      return true; // 检查异常不阻断，交给截图流程本身兜底
    }
  }
}
