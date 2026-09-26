# slow_table.c is checked in (generated once by `python3 gen.py 550000 >
# slow_table.c`) rather than regenerated on every build: it is the expensive
# translation unit, and a static file means every build — and the
# nix-develop-vs-nix-build cache-sharing test (slow-compile-test) — compiles
# byte-identical input, so nixception's action digest actually lines up.
# Regenerate it (see gen.py) only to change the row count.
#
# main.c is trivial. Honors CC / CFLAGS from the environment: under reccStdenv
# the inherited $CC is already the recc-wrapped compiler, so `-c` compiles run
# on nixception with no change here.

CC     ?= cc
CFLAGS ?= -O2

.PHONY: all run clean

all: slow_demo

slow_table.o: slow_table.c
	$(CC) $(CFLAGS) -c $< -o $@

main.o: main.c
	$(CC) $(CFLAGS) -c $< -o $@

slow_demo: slow_table.o main.o
	$(CC) $(CFLAGS) -o $@ $^

run: slow_demo
	./slow_demo

clean:
	$(RM) slow_table.o main.o slow_demo
