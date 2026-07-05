cmake_minimum_required(VERSION 3.20)
project(demo)

set(FOO "bar")

function(register_test name)
  add_test(NAME ${name} COMMAND ${name})
endfunction()

if(BUILD_TESTS)
  add_subdirectory(tests)
  foreach(t IN LISTS TEST_LIST)
    register_test(${t})
  endforeach()
endif()
