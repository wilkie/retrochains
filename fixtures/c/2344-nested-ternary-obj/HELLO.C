int main(void) {
  int a;
  int b;
  int c;
  int m;
  a = 7;
  b = 12;
  c = 5;
  m = (a > b ? a : b) > c ? (a > b ? a : b) : c;
  return m;
}
