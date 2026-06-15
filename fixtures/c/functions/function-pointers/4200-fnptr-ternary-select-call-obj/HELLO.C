int dbl(int x) { return x + x; }
int neg(int x) { return -x; }
int main(void) {
  int (*fp)(int);
  int k;
  k = 1;
  fp = k ? dbl : neg;
  return fp(9);
}
