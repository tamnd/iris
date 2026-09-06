# Builds the corpus generator against a checkout of the reference implementation.
#
# This is not part of the Rust workspace and nothing in CI builds it. It runs by hand on a machine
# that has the reference built, and what it produces is committed. See README.md for why.
#
# BTRBLOCKS_ROOT is a checkout of the reference that has already been built in place, so that
# `build/libbtrblocks.a` and the vendored dependencies it needs are sitting where its own build put
# them. The paths below are the ones the reference's build produces, read off its compile and link
# lines rather than guessed, which is why they are written out here instead of found.
#
#   cmake -DCMAKE_BUILD_TYPE=Release -DBTRBLOCKS_ROOT=$HOME/ref/btrblocks ..
#   make

cmake_minimum_required(VERSION 3.20)
project(btrblocks_conformance CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

if(NOT DEFINED BTRBLOCKS_ROOT)
  message(FATAL_ERROR "set BTRBLOCKS_ROOT to a built checkout of the reference")
endif()

set(THREADS_PREFER_PTHREAD_FLAG ON)
find_package(Threads REQUIRED)

set(BTR_BUILD ${BTRBLOCKS_ROOT}/build)
set(BTR_VENDOR ${BTR_BUILD}/vendor)
set(BOOST_LIBS ${BTR_VENDOR}/boost/boost_src-src/libs)

add_executable(generate generate.cpp)

target_include_directories(generate SYSTEM PRIVATE
  ${BTRBLOCKS_ROOT}/btrblocks
  ${BTR_VENDOR}/cwida/fsst/src/fsst_src
  ${BTR_VENDOR}/lemire/fastpfor/src/fastpfor_src
  ${BTR_VENDOR}/croaring/include
  ${BOOST_LIBS}/dynamic_bitset/include
  ${BOOST_LIBS}/assert/include
  ${BOOST_LIBS}/config/include
  ${BOOST_LIBS}/container_hash/include
  ${BOOST_LIBS}/describe/include
  ${BOOST_LIBS}/mp11/include
  ${BOOST_LIBS}/type_traits/include
  ${BOOST_LIBS}/static_assert/include
  ${BOOST_LIBS}/core/include
  ${BOOST_LIBS}/throw_exception/include
  ${BOOST_LIBS}/integer/include
  ${BOOST_LIBS}/move/include
)

target_link_libraries(generate PRIVATE
  ${BTR_BUILD}/libbtrblocks.a
  ${BTR_VENDOR}/cwida/fsst/src/fsst_src-build/libfsst.a
  ${BTR_VENDOR}/lemire/fastpfor/src/fastpfor_src-build/libFastPFOR.a
  ${BTR_VENDOR}/croaring/lib/libroaring.so
  Threads::Threads
)

# The reference compiles with -march=native and its static library carries objects built that way, so
# the generator has to agree with it about what the target is. Anything else is a header inlined at
# one instruction set into a call that returns at another.
target_compile_options(generate PRIVATE -march=native)
