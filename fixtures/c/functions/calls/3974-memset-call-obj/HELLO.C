void *memset(void *, int, unsigned);
int main(void) {
  char a[4];
  memset(a, 0, 4);
  return a[0];
}
