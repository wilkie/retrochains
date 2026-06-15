int inc(int x) { return x + 1; }
int sqr(int x) { return x * x; }
int main(void) {
  return sqr(inc(4));
}
