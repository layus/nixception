#include "greet.h"
#include <iostream>

// Implement the C linkage greeting function declared in greet.h.
//
// greet.h declares the function with extern "C" when compiled as C++ so that
// it has C linkage. We provide the matching definition here.
extern "C" void print_greeting(void) {
    std::cout << "Hello from greet()" << std::endl;
}
