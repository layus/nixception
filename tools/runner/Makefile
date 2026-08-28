# tools/runner/Makefile
#
# Builds the "runner" — the C++ builder used for every REAPI action
# derivation that nixception creates (see runner.cpp for what it does).
#
# This repo's own runner.nix does *not* use this Makefile — it compiles
# runner.cpp directly, since it's embedded verbatim into the nixception
# binary via include_str! for the server's self-build path and must stay
# free of an external Makefile dependency. This Makefile is for standalone
# use (`make -C tools/runner`) and for downstream consumers (e.g. the
# nixpkgs packaging) who want a make-based build and provide their own .nix
# wrapper around it.
#
# Intentionally minimal: $(CXX)/$(CXXFLAGS) and any -isystem include paths
# (e.g. for nlohmann_json) come from the environment.

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
