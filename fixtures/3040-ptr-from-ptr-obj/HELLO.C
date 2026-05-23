int v = 42;
int *src = &v;
int *dst = &v;
int *get(void) {
  return dst;
}
