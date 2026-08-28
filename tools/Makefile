# tools/Makefile
#
# Builds the "runner" — the C++ builder used for every REAPI action
# derivation that nixception creates (see runner.cpp for what it does).
#
# Intentionally minimal: $(CXX)/$(CXXFLAGS) and any -isystem include paths
# (e.g. for nlohmann_json) come from the environment, so this works both
# under a plain Nix stdenv build (see runner.nix) and with an ad-hoc
# system compiler for local hacking.

CXX ?= c++
CXXFLAGS ?= -std=c++17 -O2 -Wall -Wextra

.PHONY: all install clean

all: runner

runner: runner.cpp
	$(CXX) $(CXXFLAGS) -o $@ $<

install: runner
	install -D -m755 runner $(out)/bin/runner

clean:
	rm -f runner
