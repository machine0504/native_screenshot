# native_screenshot — Linux 构建集成。
#
# 本文件会被宿主 App 的 Flutter 构建自动 add_subdirectory（见 App 的
# flutter/generated_plugins.cmake）。它负责：构建跨平台 Rust 截图 helper，并把产物
# 安装到 App bundle 根目录（与可执行文件同级），供纯 Dart 侧以子进程方式启动。
#
# 宿主 App 端**无需任何配置**：删除 App 自己 CMakeLists 里的 rust 集成后，这里统一负责。
cmake_minimum_required(VERSION 3.13)
project(native_screenshot_library LANGUAGES NONE)

# rust 源码在插件根下（linux/ 的上一级）。
get_filename_component(RUST_SNIP_DIR "${CMAKE_CURRENT_SOURCE_DIR}/../rust_screenshot" ABSOLUTE)
find_program(CARGO_EXECUTABLE cargo)

if(CARGO_EXECUTABLE AND EXISTS "${RUST_SNIP_DIR}/Cargo.toml")
  set(RUST_SNIP_EXE "${RUST_SNIP_DIR}/target/release/rust_screenshot")
  add_custom_target(rust_screenshot_helper ALL
    COMMAND "${CARGO_EXECUTABLE}" build --release --manifest-path "${RUST_SNIP_DIR}/Cargo.toml"
    WORKING_DIRECTORY "${RUST_SNIP_DIR}"
    BYPRODUCTS "${RUST_SNIP_EXE}"
    COMMENT "[native_screenshot] Building rust_screenshot helper (release)"
    VERBATIM)
  # CMAKE_INSTALL_PREFIX 由宿主 App 的顶层 CMakeLists 设为 bundle 目录；装到根即与
  # 可执行文件同级，native_screenshot 的 _resolveHelper 据此就近定位。
  # 注意：DESTINATION 必须用相对路径 "."，让 CMake 自动拼接 CMAKE_INSTALL_PREFIX 与
  # DESTDIR。若写成绝对的 ${CMAKE_INSTALL_PREFIX}，当该值在子项目里未被宿主覆盖（回退到
  # 默认 /usr/local）时会直接写绝对路径而被系统拒绝（Permission denied）。
  install(PROGRAMS "${RUST_SNIP_EXE}"
    DESTINATION "."
    COMPONENT Runtime)
else()
  message(WARNING "[native_screenshot] cargo 未找到或 crate 缺失；截图 helper 不会被打包。"
                  "请确保构建机已安装 Rust 工具链（cargo 在 PATH 中）。")
endif()

# FFI 插件约定：上层会读取 ${PLUGIN}_bundled_libraries；helper 由上面的 install() 直接
# 安装到 bundle 根，不走 bundled_libraries（那会被装进 lib/ 子目录），故此处置空。
set(native_screenshot_bundled_libraries "" PARENT_SCOPE)
