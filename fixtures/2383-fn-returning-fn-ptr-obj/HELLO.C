int doubled(int x) { return x * 2; }
int (*pick(void))(int) {
  return doubled;
}
int main(void) {
  int (*f)(int);
  f = pick();
  return f(7);
}
