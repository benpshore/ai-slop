cmake_minimum_required(VERSION 3.25)
project(text_processing_engine LANGUAGES CXX OBJCXX)

set(CMAKE_EXPORT_COMPILE_COMMANDS ON)
set(CMAKE_C_STANDARD 17)
set(CMAKE_CXX_STANDARD 20)
enable_testing()

function(add_program target source)
  add_executable(${target} ${source})
  target_compile_options(${target} PRIVATE -Wall -Wextra -Werror)
  add_test(NAME ${target} COMMAND ${target})
endfunction()

add_program(text_processing_engine_cpp src/main.cpp)
add_program(text_processing_engine_objcpp src/main.mm)
target_link_libraries(text_processing_engine_objcpp PRIVATE "-framework Foundation")
