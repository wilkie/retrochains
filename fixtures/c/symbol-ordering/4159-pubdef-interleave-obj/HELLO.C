int f1(void) { return 1; }
int g1 = 10;
int f2(void) { return 2; }
int g2 = 20;
int main(void) {
  int a = f1();
  int b = f2();
  return a + b + g1 + g2;
}
