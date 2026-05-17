int f(long x) { return 0; }
long g;
int main(void) {
  long *p = &g;
  f(*p);
  return 0;
}
