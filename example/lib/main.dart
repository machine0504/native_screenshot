import 'dart:typed_data';

import 'package:flutter/material.dart';
import 'package:native_screenshot/native_screenshot.dart';
import 'package:window_manager/window_manager.dart';

Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
  // “最小化窗口截图”需要 window_manager 就绪。
  await windowManager.ensureInitialized();
  runApp(const MyApp());
}

class MyApp extends StatelessWidget {
  const MyApp({super.key});

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: '截图演示',
      theme: ThemeData(colorSchemeSeed: Colors.blue, useMaterial3: true),
      home: const DemoPage(),
    );
  }
}

class DemoPage extends StatefulWidget {
  const DemoPage({super.key});

  @override
  State<DemoPage> createState() => _DemoPageState();
}

class _DemoPageState extends State<DemoPage> {
  Uint8List? _png;
  String _status = '点击下方按钮开始截图（框选后按 Enter 确认，Esc 取消）';
  bool _busy = false;

  Future<void> _capture({required bool minimize}) async {
    if (_busy) return;
    setState(() {
      _busy = true;
      _status = '截图中…';
    });
    final png = await NativeScreenshot.captureRegion(
      minimizeWindow: minimize,
      onPermissionDenied: () {
        if (!mounted) return;
        ScaffoldMessenger.of(context).showSnackBar(
          const SnackBar(
            content: Text('需要在「系统设置 > 隐私与安全性 > 屏幕录制」中授权本示例，然后重新打开'),
          ),
        );
      },
    );
    if (!mounted) return;
    setState(() {
      _png = png;
      _busy = false;
      _status = png == null
          ? '已取消 / 无结果'
          : '截图成功：${png.length} 字节（已写入系统剪贴板）';
    });
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(title: const Text('截图演示')),
      body: Center(
        child: SingleChildScrollView(
          padding: const EdgeInsets.all(24),
          child: Column(
            mainAxisAlignment: MainAxisAlignment.center,
            children: [
              Text(_status, textAlign: TextAlign.center),
              const SizedBox(height: 20),
              Wrap(
                spacing: 12,
                runSpacing: 12,
                alignment: WrapAlignment.center,
                children: [
                  FilledButton.icon(
                    onPressed: _busy ? null : () => _capture(minimize: false),
                    icon: const Icon(Icons.crop),
                    label: const Text('区域截图（保留本窗口）'),
                  ),
                  FilledButton.tonalIcon(
                    onPressed: _busy ? null : () => _capture(minimize: true),
                    icon: const Icon(Icons.screenshot_monitor),
                    label: const Text('区域截图（最小化本窗口）'),
                  ),
                ],
              ),
              const SizedBox(height: 28),
              if (_png != null)
                Container(
                  constraints:
                      const BoxConstraints(maxWidth: 680, maxHeight: 420),
                  decoration: BoxDecoration(
                    border: Border.all(color: Colors.grey.shade400),
                    borderRadius: BorderRadius.circular(8),
                  ),
                  clipBehavior: Clip.antiAlias,
                  child: Image.memory(_png!),
                ),
            ],
          ),
        ),
      ),
    );
  }
}
