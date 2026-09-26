#include <stddef.h>
#include <stdio.h>

size_t slow_table_rows(void);
extern const struct row {
  long a, b, c, d;
} slow_table[];

int main(void) {
  size_t n = slow_table_rows();
  /* Touch a few entries so the table can't be optimized away. */
  unsigned long acc = 0;
  for (size_t i = 0; i < n; i += (n / 8) + 1) {
    acc ^= (unsigned long)slow_table[i].a;
    acc += (unsigned long)slow_table[i].d;
  }
  printf("slow_table: %zu rows, checksum %lu\n", n, acc);
  return 0;
}
