int main(void) {
  int a = 0xff;
  int n = 3;
  a &= (1 << n) - 1;
  return a;
}
