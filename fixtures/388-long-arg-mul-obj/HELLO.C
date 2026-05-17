int f(long x) { return 0; }
long g;
long h;
int main(void) {
  f(g * h);
  return 0;
}
