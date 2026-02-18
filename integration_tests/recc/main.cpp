#include <iostream>

#ifndef BUILD_CONSTANT
#define BUILD_CONSTANT "default_value"
#endif

int main() {
    std::cout << BUILD_CONSTANT << std::endl;
    return 0;
}
