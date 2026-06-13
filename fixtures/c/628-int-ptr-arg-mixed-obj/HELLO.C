int f(int x, int *p) {
  *p = x;
  return x;
}
int main(void) {
  int v;
  v = 0;
  return f(42, &v);
}
