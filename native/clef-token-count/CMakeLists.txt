cmake_minimum_required(VERSION 3.18)
project(clef-token-count C CXX)

# Must match runtime/manifest.json and the catalog runtime.
set(LLAMA_CPP_COMMIT "b92761a515ea31e852e7fbc1fad5f874b46f3718")
set(LLAMA_CPP_SOURCE_DIR "" CACHE PATH "Existing llama.cpp checkout at the pinned commit (skips the download)")

# CPU-only, static: the helper only loads the vocabulary, so no GPU backend is needed.
set(BUILD_SHARED_LIBS        OFF CACHE BOOL "" FORCE)
set(GGML_NATIVE              OFF CACHE BOOL "" FORCE)
set(GGML_CUDA                OFF CACHE BOOL "" FORCE)
set(GGML_METAL               OFF CACHE BOOL "" FORCE)
set(GGML_OPENMP              OFF CACHE BOOL "" FORCE)
set(LLAMA_BUILD_COMMON       ON  CACHE BOOL "" FORCE)
set(LLAMA_BUILD_TESTS        OFF CACHE BOOL "" FORCE)
set(LLAMA_BUILD_EXAMPLES     OFF CACHE BOOL "" FORCE)
set(LLAMA_BUILD_TOOLS        ON  CACHE BOOL "" FORCE)
set(LLAMA_BUILD_SERVER       ON  CACHE BOOL "" FORCE)
set(LLAMA_BUILD_UI           OFF CACHE BOOL "" FORCE)
set(LLAMA_USE_PREBUILT_UI    OFF CACHE BOOL "" FORCE)
set(LLAMA_OPENSSL            OFF CACHE BOOL "" FORCE)
set(LLAMA_CURL               OFF CACHE BOOL "" FORCE)

if (LLAMA_CPP_SOURCE_DIR)
    add_subdirectory(${LLAMA_CPP_SOURCE_DIR} llama.cpp EXCLUDE_FROM_ALL)
    set(LLAMA_CPP_DIR ${LLAMA_CPP_SOURCE_DIR})
else()
    include(FetchContent)
    FetchContent_Declare(llama_cpp
        GIT_REPOSITORY https://github.com/ggml-org/llama.cpp.git
        GIT_TAG        ${LLAMA_CPP_COMMIT}
        EXCLUDE_FROM_ALL)
    FetchContent_MakeAvailable(llama_cpp)
    set(LLAMA_CPP_DIR ${llama_cpp_SOURCE_DIR})
endif()

execute_process(
    COMMAND git -C ${LLAMA_CPP_DIR} rev-parse HEAD
    OUTPUT_VARIABLE LLAMA_CPP_HEAD OUTPUT_STRIP_TRAILING_WHITESPACE)
if (NOT LLAMA_CPP_HEAD STREQUAL LLAMA_CPP_COMMIT)
    message(FATAL_ERROR "llama.cpp at ${LLAMA_CPP_DIR} is ${LLAMA_CPP_HEAD}, expected ${LLAMA_CPP_COMMIT}")
endif()

add_executable(clef-token-count main.cpp)
target_compile_features(clef-token-count PRIVATE cxx_std_17)
target_compile_definitions(clef-token-count PRIVATE CLEF_TOKEN_COUNT_LLAMA_COMMIT="${LLAMA_CPP_COMMIT}")
target_include_directories(clef-token-count PRIVATE ${LLAMA_CPP_DIR}/tools/server ${LLAMA_CPP_DIR}/tools/mtmd)
if (NOT TARGET server-context)
    message(FATAL_ERROR "llama.cpp did not define the server-context target")
endif()
target_link_libraries(clef-token-count PRIVATE server-context)
install(TARGETS clef-token-count RUNTIME DESTINATION bin)

# Release binaries must run on machines without a matching C++ runtime.
if (CMAKE_CXX_COMPILER_ID STREQUAL "GNU" AND NOT APPLE AND NOT WIN32)
    target_link_options(clef-token-count PRIVATE -static-libstdc++ -static-libgcc)
endif()
