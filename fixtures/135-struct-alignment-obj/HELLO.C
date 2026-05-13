struct mixed { char c; int n; };
int main(void) {
  struct mixed m;
  m.c = 9;
  m.n = 42;
  return m.n;
}
