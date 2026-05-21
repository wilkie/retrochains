int inc(int x) { return x + 1; }
int main(void) {
  return inc(inc(inc(0)));
}
