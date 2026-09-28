import 'dart:async';
import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import 'package:flutter/foundation.dart';

/// 截图控制器 —— 驱动随包分发的 Rust 截图助手程序并取回结果。
///
/// 整套 Snipaste 式交互（冻结快照 + 框选 + 放大镜 + 取色 + 标注工具栏）由
/// 跨平台 Rust 程序 `rust_screenshot`（winit + tiny-skia）一次写成，三端通用，
/// 见包内 `SPEC.md`。
///
/// 启动方式有两种：
///   * 常驻模式（`--serve`）：进程在 App 启动后常驻、框架保持「热」状态，点击截图
///     时通过 stdin/stdout 行协议秒开，避免每次都付出「新进程 + 框架冷启动」开销。
///   * 一次性模式：每次现起一个进程截一次（常驻不可用时的兜底，永远可用）。
class ScreenshotController {
  ScreenshotController._();

  static ScreenshotController? _instance;
  static ScreenshotController get instance =>
      _instance ??= ScreenshotController._();

  // ── 常驻进程状态 ──
  Process? _server;
  Completer<void>? _ready; // READY 握手
  final List<Completer<String>> _pending = []; // FIFO：每次 capture 一个
  bool _capturing = false; // 串行化，避免同时弹两个遮罩
  Process? _oneShotProcess; // 一次性截图子进程（可被取消）

  /// 最近一次截图的「失败原因」——指基础设施类错误（helper 未找到 / 启动失败 /
  /// 进程异常退出 / 捕获或遮罩出错），**区别于用户主动取消**。
  ///
  /// 用户取消（CANCELLED）保持为 `null`，调用方据此只在真正失败时给出提示，
  /// 避免把「用户主动取消」也弹成错误。每次 [captureRegionOnDesktopOverlay]
  /// 真正开始时清空。
  String? lastFailureReason;

  /// 桌面端优先走 `--serve` 常驻模式：App 启动时预热一次，后续截图复用同一进程，
  /// 避免每次截图都新起子进程（macOS 上可消除程序坞“闪一下又消失”）。
  /// 常驻不可用时自动回退一次性模式。
  bool get _useResidentServer =>
      Platform.isWindows || Platform.isLinux || Platform.isMacOS;

  /// 统信/UOS 等 Wayland 会话下，fcitx 的 Wayland 输入法通道往往不给我们这个原生
  /// Wayland 覆盖层送组字（只发空 preedit、从不 Commit），导致文字工具打不了中文
  /// （英文因走原始按键兜底反而正常）；同机的 XWayland(X11) 程序走 XIM 却能打中文。
  ///
  /// 因此在 Linux 上启动 helper 子进程时，把 `WAYLAND_DISPLAY`/`WAYLAND_SOCKET`
  /// 置空 —— winit 与我们的 overlay 都把「空值」视为未设置，等价于
  /// `env -u WAYLAND_DISPLAY`，让 helper 回退到 XWayland 并复用系统已配好的 XIM
  /// （`XMODIFIERS`/`GTK_IM_MODULE` 等由父进程环境继承，不同发行版各取所需）。
  /// 只作用于 helper 子进程，主程序仍照常跑 Wayland。抓屏走 XDG 门户，不受影响。
  Map<String, String>? get _helperEnvironment {
    if (!Platform.isLinux) return null;
    return const {
      'WAYLAND_DISPLAY': '',
      'WAYLAND_SOCKET': '',
    };
  }

  /// 预热：在 App 启动早期调用一次，提前拉起常驻助手并加载框架，
  /// 让用户「第一次点击」也能秒开。失败无副作用（截图时会回退一次性模式）。
  Future<void> warmUp() async {
    if (!_useResidentServer) return;
    try {
      await _ensureServer();
    } catch (e) {
      debugPrint('[ScreenshotController] warmUp skipped: $e');
    }
  }

  /// 显示原生遮罩并框选 + 标注，确认后返回选区 PNG（同时已写入系统剪贴板）；
  /// 用户取消或出错返回 `null`。桌面端优先走常驻进程，失败自动回退一次性进程。
  Future<Uint8List?> captureRegionOnDesktopOverlay() async {
    if (_capturing) {
      debugPrint('[ScreenshotController] capture already in progress, ignored');
      return null;
    }
    _capturing = true;
    lastFailureReason = null;
    try {
      if (!_useResidentServer) {
        return await _captureOneShot();
      }
      try {
        return await _captureViaServer();
      } catch (e) {
        debugPrint('[ScreenshotController] resident path failed ($e); '
            'falling back to one-shot');
        return await _captureOneShot();
      }
    } finally {
      _capturing = false;
    }
  }

  /// 取消进行中的一张截图并释放串行锁（主窗口恢复时务必调用，避免后续点击无效）。
  void cancelActiveCapture() {
    _capturing = false;
    final p = _oneShotProcess;
    _oneShotProcess = null;
    if (p != null) {
      try {
        p.kill();
      } catch (_) {}
    }
  }

  /// 释放常驻进程（App 退出时可调用）。
  Future<void> dispose() async {
    final p = _server;
    _server = null;
    _ready = null;
    if (p != null) {
      try {
        p.stdin.writeln('QUIT');
        await p.stdin.flush();
      } catch (_) {}
      p.kill();
    }
  }

  // ─────────────────────────── resident server ───────────────────────────

  /// 通过常驻进程截一次图；基础设施失败（无法启动/进程死亡）抛异常以触发回退，
  /// 而用户主动取消（CANCELLED）正常返回 `null`，不会回退（否则会重开遮罩）。
  Future<Uint8List?> _captureViaServer() async {
    await _ensureServer();
    final server = _server;
    if (server == null) {
      throw StateError('resident helper unavailable');
    }

    final completer = Completer<String>();
    _pending.add(completer);
    try {
      server.stdin.writeln('CAPTURE');
      await server.stdin.flush();
    } catch (e) {
      _pending.remove(completer);
      throw StateError('write to helper failed: $e');
    }

    // 用户可能会长时间标注，给足时间；进程若死亡会由 _onServerExit 提前以错误结束。
    final line = await completer.future.timeout(
      const Duration(minutes: 30),
      onTimeout: () => throw TimeoutException('helper capture timed out'),
    );
    return _decodeResultLine(line);
  }

  /// 确保常驻进程已启动且完成 READY 握手。
  Future<void> _ensureServer() async {
    if (_server != null && _ready != null) {
      await _ready!.future.timeout(const Duration(seconds: 8));
      return;
    }
    final exe = _resolveHelper();
    if (exe == null) {
      throw StateError('helper binary not found');
    }

    final Process p;
    try {
      p = await Process.start(exe, const ['--serve'],
          environment: _helperEnvironment);
    } catch (e) {
      throw StateError('failed to start helper: $e');
    }
    _server = p;
    final ready = Completer<void>();
    _ready = ready;

    p.stdout
        .transform(utf8.decoder)
        .transform(const LineSplitter())
        .listen(_onServerLine, onError: (_) {}, onDone: () {});
    // Drain stderr so the pipe never blocks the child.
    p.stderr.transform(utf8.decoder).transform(const LineSplitter()).listen(
          (l) => debugPrint('[rust_screenshot] $l'),
          onError: (_) {},
        );
    unawaited(p.exitCode.then((code) => _onServerExit(code)));

    try {
      await ready.future.timeout(const Duration(seconds: 8));
    } catch (e) {
      _onServerExit(-1);
      throw StateError('helper did not become ready: $e');
    }
  }

  void _onServerLine(String line) {
    final t = line.trim();
    if (t.isEmpty) return;
    if (t == 'READY') {
      final r = _ready;
      if (r != null && !r.isCompleted) r.complete();
      return;
    }
    // Result line for the oldest in-flight capture.
    if (_pending.isNotEmpty) {
      final c = _pending.removeAt(0);
      if (!c.isCompleted) c.complete(t);
    }
  }

  void _onServerExit(int code) {
    if (_server != null) {
      debugPrint('[ScreenshotController] resident helper exited ($code)');
    }
    _server = null;
    final r = _ready;
    _ready = null;
    if (r != null && !r.isCompleted) {
      r.completeError(StateError('helper exited'));
    }
    // Fail any in-flight captures so the caller can fall back.
    final pending = List<Completer<String>>.from(_pending);
    _pending.clear();
    for (final c in pending) {
      if (!c.isCompleted) c.completeError(StateError('helper exited'));
    }
  }

  // ─────────────────────────── one-shot fallback ───────────────────────────

  Future<Uint8List?> _captureOneShot() async {
    final exe = _resolveHelper();
    if (exe == null) {
      debugPrint('[ScreenshotController] helper binary not found');
      lastFailureReason = '未找到截图组件（rust_screenshot），安装包可能未正确打包该组件';
      return null;
    }

    Process? process;
    final stdoutBuffer = StringBuffer();
    final stderrBuffer = StringBuffer();

    try {
      debugPrint('[ScreenshotController] launching one-shot helper: $exe');
      process = await Process.start(exe, const <String>[],
          environment: _helperEnvironment);
      _oneShotProcess = process;

      final stdoutDone = process.stdout
          .transform(utf8.decoder)
          .transform(const LineSplitter())
          .listen(stdoutBuffer.writeln);
      final stderrDone = process.stderr
          .transform(utf8.decoder)
          .transform(const LineSplitter())
          .listen((line) {
        stderrBuffer.writeln(line);
        debugPrint('[rust_screenshot] $line');
      });

      final exitCode = await process.exitCode;
      await stdoutDone.cancel();
      await stderrDone.cancel();

      final stdoutStr = stdoutBuffer.toString();
      final line = stdoutStr
          .split('\n')
          .map((l) => l.trim())
          .where((l) => l.isNotEmpty)
          .lastOrNull;
      if (line == null) {
        debugPrint('[ScreenshotController] helper produced no output '
            '(exit $exitCode)');
        final err = stderrBuffer.toString().trim();
        lastFailureReason = err.isNotEmpty
            ? '截图组件启动失败（退出码 $exitCode）：${_lastLine(err)}'
            : '截图组件未返回结果（退出码 $exitCode），可能缺少运行库或与桌面环境不兼容';
        return null;
      }
      return await _decodeResultLine(line, stderr: stderrBuffer.toString());
    } catch (e, st) {
      debugPrint('[ScreenshotController] launch error: $e\n$st');
      lastFailureReason = '启动截图组件失败：$e';
      return null;
    } finally {
      if (identical(_oneShotProcess, process)) {
        _oneShotProcess = null;
      }
    }
  }

  // ─────────────────────────── shared helpers ───────────────────────────

  /// 解析助手输出的结果行：`OK <path>` / `CANCELLED` / `ERROR <msg>`。
  Future<Uint8List?> _decodeResultLine(String line, {Object? stderr}) async {
    if (line.startsWith('OK ')) {
      final path = line.substring(3).trim();
      final file = File(path);
      if (await file.exists()) {
        final bytes = await file.readAsBytes();
        unawaited(file.delete().catchError((_) => file));
        debugPrint('[ScreenshotController] capture ok: ${bytes.length} bytes');
        return bytes;
      }
      debugPrint('[ScreenshotController] helper png missing: $path');
      lastFailureReason = '截图文件缺失：$path';
      return null;
    }
    if (line == 'CANCELLED') {
      debugPrint('[ScreenshotController] capture cancelled');
      return null; // 用户主动取消，不视为失败
    }
    debugPrint('[ScreenshotController] helper error: $line'
        '${stderr != null ? ' / stderr=$stderr' : ''}');
    // 形如 "ERROR <message>"，去掉前缀给用户更友好的提示。
    lastFailureReason =
        line.startsWith('ERROR ') ? line.substring(6).trim() : line;
    return null;
  }

  /// 取多行文本的最后一行非空内容（用于把 stderr 的末行作为简短错误提示）。
  String _lastLine(String text) {
    final lines = text
        .split('\n')
        .map((l) => l.trim())
        .where((l) => l.isNotEmpty)
        .toList();
    return lines.isEmpty ? text.trim() : lines.last;
  }

  String get _binName =>
      Platform.isWindows ? 'rust_screenshot.exe' : 'rust_screenshot';

  /// 解析助手可执行文件位置：环境变量覆盖 → 发布包内 Helpers mini-app →
  /// 与主程序同目录（旧布局） → 开发期在工程树中向上查找 target/{release,debug}。
  String? _resolveHelper() {
    final override = Platform.environment['NATIVE_SCREENSHOT_HELPER'];
    if (override != null && override.isNotEmpty && File(override).existsSync()) {
      return override;
    }

    final candidates = <String>[];

    // 1) 发布包：Contents/Helpers/rust_screenshot.app（macOS 推荐布局）。
    final exeDir = File(Platform.resolvedExecutable).parent.path;
    final contentsDir = Directory(exeDir).parent.path;
    candidates.add(_join(
      contentsDir,
      'Helpers',
      'rust_screenshot.app',
      'Contents',
      'MacOS',
      _binName,
    ));

    // 2) 与主程序同目录（旧版发布包 / 开发期 flutter run 嵌入）。
    candidates.add(_join(exeDir, _binName));

    // 3) 开发期：从当前目录与主程序目录分别向上查找工程内的构建产物。
    for (final start in <String>[Directory.current.path, exeDir]) {
      for (final profile in const ['release', 'debug']) {
        candidates.addAll(_ancestorCandidates(start, profile));
      }
    }

    for (final c in candidates) {
      if (File(c).existsSync()) return c;
    }
    return null;
  }

  /// 自 [start] 向上至多 12 层，拼出 `<ancestor>/rust_screenshot/target/<profile>/<bin>`
  /// 以及 `<ancestor>/desktop/rust_screenshot/target/<profile>/<bin>`。
  List<String> _ancestorCandidates(String start, String profile) {
    final out = <String>[];
    var dir = Directory(start);
    for (var i = 0; i < 12; i++) {
      // 封装后：helper 源码在插件内 package/native_screenshot/rust_screenshot。
      out.add(_join(
          _join(dir.path, 'package', 'native_screenshot', 'rust_screenshot',
              'target', profile),
          _binName));
      // 兼容旧布局（同级 rust_screenshot 或 desktop/rust_screenshot）。
      out.add(_join(dir.path, 'rust_screenshot', 'target', profile, _binName));
      out.add(_join(dir.path, 'desktop', 'rust_screenshot', 'target', profile, _binName));
      final parent = dir.parent;
      if (parent.path == dir.path) break;
      dir = parent;
    }
    return out;
  }

  String _join(String a,
      [String? b, String? c, String? d, String? e, String? f]) {
    final sep = Platform.pathSeparator;
    return [a, b, c, d, e, f].whereType<String>().join(sep);
  }
}
