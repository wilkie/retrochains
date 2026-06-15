int main(void) {
  volatile int x = 5;
  int a = x;
  int b = x;
  return a + b;
}
