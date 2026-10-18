cmake_minimum_required(VERSION 3.10)
project(fixture C CXX)

add_executable(fixture_c cmake/main.c)
add_executable(fixture_cxx cmake/main.cpp)

enable_testing()
add_test(NAME c_runs COMMAND fixture_c)
add_test(NAME cxx_runs COMMAND fixture_cxx)
