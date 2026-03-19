/*
Multi-file C++ example for the `recc` integration test.

This repository file (`test/main.cpp`) now contains the actual `main.cpp`
implementation plus, as comments, the contents of the additional source
and header files and a `Makefile` you should create alongside it under the
same `test/` directory.

Goal:
- Provide a small but not-trivial C++ app with multiple .cpp/.h files.
- Provide a generic `Makefile` that uses `CC`, `CXX`, `CXXFLAGS` and friends
  (does NOT rely on `recc`).
- The integration test can be updated to run `make` with `CC` set to `recc`
  (e.g., `env CC=${buildbox}/bin/recc make`) and validate the produced binary.

Files to create under `integration_tests/recc/test/`:
- `Makefile`         (contents shown below)
- `main.cpp`         (this file — real implementation below)
- `greet.h`          (shown below)
- `greet.cpp`        (shown below)
- `util.h`           (shown below)
- `util.cpp`         (shown below)

Makefile (create as `test/Makefile`):
---------------------------------------------------------
# Generic Makefile using common variables
CC ?= gcc
CXX ?= g++
CXXFLAGS ?= -std=c++17 -O2 -Wall -Wextra
CPPFLAGS ?=
LDFLAGS ?=
SRCS = main.cpp greet.cpp util.cpp
OBJS = $(SRCS:.cpp=.o)
TARGET = demo_app

.PHONY: all clean distclean run

all: $(TARGET)

$(TARGET): $(OBJS)
	$(CXX) $(CXXFLAGS) $(LDFLAGS) -o $@ $^

# Generic rule for building object files
%.o: %.cpp
	$(CXX) $(CXXFLAGS) $(CPPFLAGS) -c $< -o $@

run: $(TARGET)
	./$(TARGET)

clean:
	$(RM) $(OBJS) $(TARGET)

distclean: clean
---------------------------------------------------------

Notes:
- To use `recc` as the compiler driver for testing, the integration test should
  set `CC` (and optionally `CXX`) to the `recc` wrapper. Example (shell):
    env CC=/path/to/recc CXX=/path/to/recc make
  or, if the test wants to set only `CC` and let `make` use `g++` for C++
  compilation, set both `CC` and `CXX` to `recc`.

Integration-test validation idea (what the Nix test should do after build):
- Run `./demo_app` and check stdout contains:
    Build constant: 42
    Hello from greet()
    Sum(2,3) = 5

Below are the source files. `main.cpp`'s real implementation follows the
commented file listings, so you can split them into separate files as shown.

---------------------------------------------------------
File: greet.h
---------------------------------------------------------
#ifndef GREET_H
#define GREET_H

void print_greeting();

#endif // GREET_H

---------------------------------------------------------
File: greet.cpp
---------------------------------------------------------
#include <iostream>
#include "greet.h"

void print_greeting() {
    std::cout << "Hello from greet()" << std::endl;
}

---------------------------------------------------------
File: util.h
---------------------------------------------------------
#ifndef UTIL_H
#define UTIL_H

int add(int a, int b);

#endif // UTIL_H

---------------------------------------------------------
File: util.cpp
---------------------------------------------------------
#include "util.h"

int add(int a, int b) {
    return a + b;
}

---------------------------------------------------------
Now: the actual `main.cpp` implementation (this file). It uses the headers
above and is intentionally small while composing a multi-file program.
---------------------------------------------------------
*/

#include <iostream>
#include "greet.h"
#include "util.h"

#ifndef BUILD_CONSTANT
#define BUILD_CONSTANT 0
#endif

int main() {
    // Print the build-time constant to make it easy for the integration test
    // to validate that a compile-time define (e.g. -DBUILD_CONSTANT=42) was
    // propagated through the compilation pipeline.
    std::cout << "Build constant: " << BUILD_CONSTANT << std::endl;

    // Call into another module
    print_greeting();

    // Use a tiny utility function implemented in another translation unit
    int a = 2, b = 3;
    std::cout << "Sum(" << a << "," << b << ") = " << add(a, b) << std::endl;

    return 0;
}
