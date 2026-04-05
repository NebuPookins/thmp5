cmake_minimum_required(VERSION 3.16)
project(taglib-helper LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)

set(TAGLIB_ROOT "$ENV{TAGLIB_ROOT}" CACHE PATH "TagLib installation prefix")

find_path(TAGLIB_INCLUDE_DIR
  NAMES taglib/tag.h
  HINTS
    ${TAGLIB_ROOT}
  PATH_SUFFIXES
    include
)

find_library(TAGLIB_LIBRARY
  NAMES tag taglib taglibd
  HINTS
    ${TAGLIB_ROOT}
  PATH_SUFFIXES
    lib
    bin
)

if(TAGLIB_INCLUDE_DIR AND TAGLIB_LIBRARY)
  add_library(taglib_local UNKNOWN IMPORTED)
  set_target_properties(taglib_local PROPERTIES
    IMPORTED_LOCATION "${TAGLIB_LIBRARY}"
    INTERFACE_INCLUDE_DIRECTORIES "${TAGLIB_INCLUDE_DIR}"
  )
  set(TAGLIB_TARGET taglib_local)
else()
  find_package(PkgConfig QUIET)
  if(PkgConfig_FOUND)
    pkg_check_modules(TAGLIB QUIET IMPORTED_TARGET taglib)
  endif()

  if(TARGET PkgConfig::TAGLIB)
    set(TAGLIB_TARGET PkgConfig::TAGLIB)
  else()
    message(FATAL_ERROR
      "Could not find TagLib. Set TAGLIB_ROOT to the TagLib install prefix, "
      "or install TagLib so pkg-config can find it."
    )
  endif()
endif()

add_executable(taglib-helper main.cpp)
target_link_libraries(taglib-helper PRIVATE ${TAGLIB_TARGET})

if(MSVC)
  target_compile_options(taglib-helper PRIVATE /W4)
else()
  target_compile_options(taglib-helper PRIVATE -Wall -Wextra -Wpedantic)
endif()
