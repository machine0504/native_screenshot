# native_screenshot — Windows 构建集成。
#
# 本文件会被宿主 App 的 Flutter 构建自动 add_subdirectory（见 App 的
# flutter/generated_plugins.cmake）。它负责：构建跨平台 Rust 截图 helper，并把
# rust_screenshot.exe 安装到 App 可执行文件同目录，供纯 Dart 侧以子进程方式启动。
#
# 宿主 App 端**无需任何配置**（这也补齐了此前 Windows 打包未拷贝 helper 的缺口）。
cmake_minimum_required(VERSION 3.14)
project(native_screenshot_library LANGUAGES NONE)

# rust 源码在插件根下（windows/ 的上一级）。
get_filename_component(RUST_SNIP_DIR "${CMAKE_CURRENT_SOURCE_DIR}/../rust_screenshot" ABSOLUTE)
find_program(CARGO_EXECUTABLE cargo)

if(CARGO_EXECUTABLE AND EXISTS "${RUST_SNIP_DIR}/Cargo.toml")
  set(RUST_SNIP_EXE "${RUST_SNIP_DIR}/target/release/rust_screenshot.exe")
  add_custom_target(rust_screenshot_helper ALL
    COMMAND "${CARGO_EXECUTABLE}" build --release --manifest-path "${RUST_SNIP_DIR}/Cargo.toml"
    WORKING_DIRECTORY "${RUST_SNIP_DIR}"
    BYPRODUCTS "${RUST_SNIP_EXE}"
    COMMENT "[native_screenshot] Building rust_screenshot helper (release)"
    VERBATIM)
  # Windows 上 CMAKE_INSTALL_PREFIX 是 $<TARGET_FILE_DIR:...> 生成器表达式。
  # 插件里直接 install(DESTINATION ".") 不会在配置期展开，安装时会变成字面路径失败。
  # 走 Flutter 标准的 bundled_libraries：由宿主在设置好前缀后统一 install 到 exe 同级。
  set(native_screenshot_bundled_libraries "${RUST_SNIP_EXE}" PARENT_SCOPE)
else()
  message(WARNING "[native_screenshot] cargo 未找到或 crate 缺失；截图 helper 不会被打包。"
                  "请确保构建机已安装 Rust 工具链（cargo 在 PATH 中）。")
  set(native_screenshot_bundled_libraries "" PARENT_SCOPE)
endif()
