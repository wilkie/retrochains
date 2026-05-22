int main(void) {
  int a;
  int b;
  int r;
  a = 5;
  b = 7;
  r = a > b ? a++ : b++;
  return r + a + b;
}
