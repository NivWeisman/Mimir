# Mimir slang sidecar build.
#
# Out of the cargo workspace on purpose: contributors hacking on the Rust
# side don't need a C++ toolchain, and the binary is shipped per-platform
# from CI rather than built per-machine.
#
# Build (out-of-source, into ./build):
#
#   cmake -G Ninja -S . -B build -DCMAKE_BUILD_TYPE=Release
#   cmake --build build
#
# Output:  ./build/mimir-slang-sidecar
#
# First configure pulls slang (~30 MB) and nlohmann_json via FetchContent.
# Subsequent configures reuse the cached sources under ./build/_deps/.

cmake_minimum_required(VERSION 3.20)
project(mimir-slang-sidecar
    VERSION      0.1.1
    DESCRIPTION  "NDJSON-over-stdio slang elaborator for the Mimir LSP"
    LANGUAGES    CXX
)

# Slang requires C++20.
set(CMAKE_CXX_STANDARD          20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS        OFF)

# Position-independent code is required by some of slang's transitive deps.
set(CMAKE_POSITION_INDEPENDENT_CODE ON)

# Default to a Release build if the user didn't pick one. Debug slang +
# debug sidecar adds 100s of MB of artifacts and is much slower; Release
# is what we want unless someone is debugging slang itself.
if(NOT CMAKE_BUILD_TYPE AND NOT CMAKE_CONFIGURATION_TYPES)
    set(CMAKE_BUILD_TYPE Release CACHE STRING "" FORCE)
endif()

include(FetchContent)

# --- nlohmann_json -------------------------------------------------------
# Used for JSON encoding/decoding on the wire. Header-only, fast.
FetchContent_Declare(
    nlohmann_json
    GIT_REPOSITORY https://github.com/nlohmann/json.git
    GIT_TAG        v3.11.3
    GIT_SHALLOW    TRUE
)

# --- slang ---------------------------------------------------------------
# We don't need slang's CLI driver, tests, docs, or install rules. Turning
# them off shrinks the build by a noticeable amount and keeps `cmake
# --build` from running test compilations we don't care about.
set(SLANG_INCLUDE_TOOLS    OFF CACHE BOOL "" FORCE)
set(SLANG_INCLUDE_TESTS    OFF CACHE BOOL "" FORCE)
set(SLANG_INCLUDE_DOCS     OFF CACHE BOOL "" FORCE)
set(SLANG_INCLUDE_INSTALL  OFF CACHE BOOL "" FORCE)
set(SLANG_INCLUDE_PYLIB    OFF CACHE BOOL "" FORCE)

FetchContent_Declare(
    slang
    GIT_REPOSITORY https://github.com/MikePopoloski/slang.git
    GIT_TAG        v10.0
    GIT_SHALLOW    TRUE
)

FetchContent_MakeAvailable(nlohmann_json slang)

# --- the sidecar ---------------------------------------------------------
add_executable(mimir-slang-sidecar
    src/main.cpp
)

target_link_libraries(mimir-slang-sidecar PRIVATE
    slang::slang
    nlohmann_json::nlohmann_json
)

# Enable extra warnings on the sidecar code only — slang itself sets its
# own warning level and we don't want to fight with it.
if(CMAKE_CXX_COMPILER_ID MATCHES "GNU|Clang")
    target_compile_options(mimir-slang-sidecar PRIVATE
        -Wall -Wextra -Wpedantic
    )
endif()
