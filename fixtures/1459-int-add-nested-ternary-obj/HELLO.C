int main(void) {
  int a = 5;
  int b = 2;
  int c = 3;
  a += b > c ? 10 : b < c ? 20 : 0;
  return a;
}
