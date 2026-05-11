int main(void) {
  int a = 0;
  int b = 1;
  int c = 2;
  int d = 3;
  while (a < 10) {
    b = b + 1;
    c = c + 1;
    d = d + 1;
    a = a + 1;
  }
  return a + b + c + d;
}
