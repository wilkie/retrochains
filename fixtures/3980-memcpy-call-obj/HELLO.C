void *memcpy(void *, void *, unsigned);
int main(void) {
  char a[4];
  char b[4] = { 1, 2, 3, 4 };
  memcpy(a, b, 4);
  return a[0];
}
