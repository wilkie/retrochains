extern int printf(const char *fmt, ...);
int main(void) {
  int i = 5;
  long l = 100000L;
  double d = 3.14;
  printf("%d %ld %f\n", i, l, d);
  return 0;
}
