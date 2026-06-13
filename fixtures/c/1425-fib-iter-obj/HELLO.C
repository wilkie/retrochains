int main(void) {
  int a = 0;
  int b = 1;
  int t;
  int i;
  for (i = 0; i < 5; i++) {
    t = a + b;
    a = b;
    b = t;
  }
  return a;
}
