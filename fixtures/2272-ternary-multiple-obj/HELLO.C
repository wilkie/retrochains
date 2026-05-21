int main(void) {
  int x = 5;
  int a = (x > 0) ? 1 : (x < 0) ? -1 : 0;
  int b = (x > 10) ? 100 : (x > 5) ? 50 : (x > 0) ? 10 : 0;
  return a + b;
}
