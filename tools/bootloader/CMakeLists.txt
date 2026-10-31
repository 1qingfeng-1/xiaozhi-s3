# 仅用于构建带 APP_ROLLBACK 的二级引导器 (不构建应用)
# 用法见 README.md / scripts/build_bootloader.sh
cmake_minimum_required(VERSION 3.16)

include($ENV{IDF_PATH}/tools/cmake/project.cmake)
project(xiaozhi_bootloader)
