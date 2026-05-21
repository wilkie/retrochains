struct M { char c; int n; };
int main(void) {
  struct M m;
  m.c = 'A';
  m.n = 100;
  return m.c + m.n;
}
