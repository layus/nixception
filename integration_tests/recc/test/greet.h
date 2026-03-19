#ifndef RECC_TEST_GREET_H
#define RECC_TEST_GREET_H

// greet.h
// Small helper header used by the integration test multi-file C++ example.
// Declares a function that prints a greeting to stdout.

#ifdef __cplusplus
extern "C" {
#endif

// Print a simple greeting message to stdout.
// Implemented in greet.cpp.
void print_greeting(void);

#ifdef __cplusplus
}
#endif

#endif // RECC_TEST_GREET_H
