#ifndef RECC_TEST_UTIL_H
#define RECC_TEST_UTIL_H

// util.h
// Small utility declarations used by the multi-file C++ example for the
// recc integration test.
//
// Provides a trivial integer addition helper implemented in util.cpp.

#ifdef __cplusplus
extern "C++" {
#endif

// Return the sum of a and b.
int add(int a, int b);

#ifdef __cplusplus
} // extern "C++"
#endif

#endif // RECC_TEST_UTIL_H
