void *realloc(void *, unsigned);
int main(void) {
  void *p = realloc(0, 64);
  return 0;
}
