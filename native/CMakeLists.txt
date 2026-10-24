cmake_minimum_required(VERSION 3.20)
project(pslc_native C CXX)

set(CHIAKI_DIR "${CMAKE_CURRENT_SOURCE_DIR}/../third_party/chiaki-ng" CACHE PATH "chiaki-ng source tree")
set(FFMPEG_DIR "" CACHE PATH "FFmpeg shared development build (include/, lib/, bin/)")

set(CHIAKI_ENABLE_TESTS OFF CACHE BOOL "" FORCE)
set(CHIAKI_ENABLE_CLI OFF CACHE BOOL "" FORCE)
set(CHIAKI_ENABLE_GUI OFF CACHE BOOL "" FORCE)
set(CHIAKI_ENABLE_SETSU OFF CACHE STRING "" FORCE)
set(CHIAKI_ENABLE_STEAMDECK_NATIVE OFF CACHE STRING "" FORCE)
set(CHIAKI_ENABLE_STEAM_SHORTCUT OFF CACHE BOOL "" FORCE)
set(CHIAKI_ENABLE_FFMPEG_DECODER OFF CACHE STRING "" FORCE)
set(CHIAKI_ENABLE_PI_DECODER OFF CACHE STRING "" FORCE)
set(CHIAKI_ENABLE_SPEEX OFF CACHE STRING "" FORCE)
set(CHIAKI_LIB_ENABLE_OPUS ON CACHE BOOL "" FORCE)
set(CHIAKI_USE_SYSTEM_JERASURE OFF CACHE STRING "" FORCE)
set(CHIAKI_USE_SYSTEM_NANOPB OFF CACHE STRING "" FORCE)
set(CHIAKI_USE_SYSTEM_CURL OFF CACHE STRING "" FORCE)

if(CMAKE_C_COMPILER_ID STREQUAL "Clang" AND MSVC)
	# gf-complete calls __cpuidex without including <intrin.h> when _MSC_VER is defined.
	add_compile_options("$<$<COMPILE_LANGUAGE:C>:/FIintrin.h>")
endif()

add_compile_definitions(MINIUPNP_STATICLIB)
add_subdirectory("${CHIAKI_DIR}" chiaki EXCLUDE_FROM_ALL)

file(STRINGS "${CHIAKI_DIR}/CMakeLists.txt" CHIAKI_VERSION_LINES REGEX "set\\(CHIAKI_VERSION_(MAJOR|MINOR|PATCH) ")
string(REGEX REPLACE ".*MAJOR ([0-9]+).*MINOR ([0-9]+).*PATCH ([0-9]+).*" "\\1.\\2.\\3" PSLC_CHIAKI_VERSION "${CHIAKI_VERSION_LINES}")

add_library(pslc_core SHARED pslc.c pslc.h)
target_compile_definitions(pslc_core PRIVATE PSLC_CHIAKI_VERSION="${PSLC_CHIAKI_VERSION}" _CRT_SECURE_NO_WARNINGS)
target_include_directories(pslc_core PRIVATE "${FFMPEG_DIR}/include")
target_link_directories(pslc_core PRIVATE "${FFMPEG_DIR}/lib")
target_link_libraries(pslc_core PRIVATE chiaki-lib avcodec avutil d3d11 dxgi iphlpapi ws2_32)

install(TARGETS pslc_core RUNTIME DESTINATION bin ARCHIVE DESTINATION lib LIBRARY DESTINATION lib)
install(FILES pslc.h DESTINATION include)
