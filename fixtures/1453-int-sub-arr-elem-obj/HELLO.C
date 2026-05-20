int main(void) {
  int a = 20;
  int b[2];
  b[0] = 5;
  b[1] = 3;
  a -= b[1];
  return a;
}
