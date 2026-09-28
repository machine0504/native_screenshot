/// Native, Snipaste-style screenshot tool for Windows / macOS / Linux.
///
/// The entire capture / selection / magnifier / annotation UI is implemented
/// natively per platform (see `SPEC.md`). Dart only exposes a thin controller
/// over the `native_screenshot` MethodChannel.
library native_screenshot;

export 'src/native_screenshot_api.dart';
export 'src/screenshot_controller.dart';
