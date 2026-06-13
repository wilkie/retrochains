int dbl(int x) { return x * 2; }
int main(void) {
  int (*fp)(int) = dbl;
  return fp(7);
}
