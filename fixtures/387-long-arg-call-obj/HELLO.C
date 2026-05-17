int f(long x) { return 0; }
long g(void);
int main(void) {
  f(g());
  return 0;
}
