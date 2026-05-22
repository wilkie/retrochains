int inc(int x) { return x + 1; }
int dbl(int x) { return x + x; }
int main(void) {
  return inc(dbl(inc(5)));
}
