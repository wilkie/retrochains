int main(void) {
  int a = 10;
  int b = 13;
  int c = 0;
  while (a < b) {
    c += a;
    a++;
  }
  return c;
}
