int f(int x) { return x + 1; }
int g(int x) { return x - 1; }
int main(void) {
  int x = 5;
  int r;
  if (x > 0) r = f(x);
  else r = g(x);
  return r;
}
