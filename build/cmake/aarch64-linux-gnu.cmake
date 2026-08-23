# CMake toolchain for cross-building native components (llama.cpp) for arm64.
#
# The Rust side does not need this — cargo cross-compiles with a linker override — but a
# CMake project needs to be told it is not building for the host, or it will happily
# produce x86-64 objects and fail at link with an error that does not mention architecture.
set(CMAKE_SYSTEM_NAME Linux)
set(CMAKE_SYSTEM_PROCESSOR aarch64)

set(CMAKE_C_COMPILER   aarch64-linux-gnu-gcc)
set(CMAKE_CXX_COMPILER aarch64-linux-gnu-g++)

# Find programs on the host, but libraries and headers only in the target sysroot.
set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)
set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE ONLY)
