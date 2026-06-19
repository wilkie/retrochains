void fa(int *p, int v) { p[1] += v; }
void fs(int *p, int v) { p[1] -= v; }
void fn(int *p, int v) { p[1] &= v; }
void fo(int *p, int v) { p[1] |= v; }
void fx(int *p, int v) { p[1] ^= v; }
int main(void) {
  int a[2];
  fa(a, 1);
  fs(a, 1);
  fn(a, 1);
  fo(a, 1);
  fx(a, 1);
  return 0;
}
