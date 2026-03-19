# Generic Makefile for a small multi-file C++ example.
# - Honors common variables: CC, CXX, CXXFLAGS, CPPFLAGS, LDFLAGS.
# - Does NOT assume `recc` or any particular compiler; it can be supplied via
#   environment (e.g. `env CC=/path/to/recc CXX=/path/to/recc make`).
# - Builds: main.cpp greet.cpp util.cpp -> demo_app
#
# Usage examples:
#   make                 # build with defaults (g++ unless overridden)
#   make CC=/path/to/recc CXX=/path/to/recc   # use recc as driver
#   make CXXFLAGS="-g -O0 -std=c++17"
#   make test            # run a basic validation of the program output

# Default compilers (can be overridden by environment)
CC ?= gcc

# If CXX isn't explicitly provided, fall back to CC (if set) or g++
ifndef CXX
  ifdef CC
    CXX := $(CC)
  else
    CXX := g++
  endif
endif

# Compilation and linking flags
CXXFLAGS ?= -std=c++17 -O2 -Wall -Wextra
CPPFLAGS ?=
LDFLAGS ?=
LDLIBS  ?=

# Sources / targets
SRCS := main.cpp greet.cpp util.cpp
OBJS := $(SRCS:.cpp=.o)
TARGET := demo_app

# Convenience
RM ?= rm -f

.PHONY: all clean distclean run test

all: $(TARGET)

$(TARGET): $(OBJS)
	$(CXX) $(CXXFLAGS) $(LDFLAGS) -o $@ $^ $(LDLIBS)

# Generic rule for building object files from C++ sources
%.o: %.cpp
	$(CXX) $(CXXFLAGS) $(CPPFLAGS) -c $< -o $@

# Run the program
run: $(TARGET)
	./$(TARGET)

# Basic functional test used by integration tests. It runs the program,
# captures output, and checks for expected lines. Exit status non-zero if
# validation fails.
test: $(TARGET)
	@echo "Running functional test..."
	@./$(TARGET) > build_output.txt 2>&1 || (echo "FAIL: program exited non-zero" && cat build_output.txt && exit 1)
	@grep -q "Build constant: 47" build_output.txt && echo "OK: build constant found" || (echo "FAIL: build constant missing" && cat build_output.txt && rm -f build_output.txt && exit 1)
	@grep -q "Hello from greet()" build_output.txt && echo "OK: greet found" || (echo "FAIL: greet missing" && cat build_output.txt && rm -f build_output.txt && exit 1)
	@grep -q "Sum(2,3) = 5" build_output.txt && echo "OK: sum found" || (echo "FAIL: sum missing" && cat build_output.txt && rm -f build_output.txt && exit 1)
	@rm -f build_output.txt
	@echo "Functional test SUCCESS"

clean:
	$(RM) $(OBJS) $(TARGET) build_output.txt

distclean: clean
	$(RM) *~ core

# Phony default target for convenience in interactive builds
.DEFAULT_GOAL := all
