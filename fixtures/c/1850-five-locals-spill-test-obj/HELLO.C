int main(void) {
  int a = 1;
  int b = 2;
  int c = 3;
  int d = 4;
  int e = 5;
  a = a + b;
  c = c + d;
  e = e + a;
  return a + c + e + b + d;
}
