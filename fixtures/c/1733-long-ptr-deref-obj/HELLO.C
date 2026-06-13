int main(void) {
  long n;
  long *p = &n;
  *p = 1000000L;
  return (int)n;
}
