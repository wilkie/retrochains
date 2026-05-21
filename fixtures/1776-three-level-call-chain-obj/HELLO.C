int sqr(int x) { return x * x; }
int dbl(int x) { return x * 2; }
int inc(int x) { return x + 1; }
int main(void) {
  return sqr(dbl(inc(2)));
}
