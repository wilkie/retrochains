int main(void) {
  int a, b;
  int r = (a = 5, b = 10, a + b);
  return r;
}
