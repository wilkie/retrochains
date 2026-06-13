int main(void) {
  int x = 0x1234;
  int a = x & 127;
  int b = x & 128;
  return a + b;
}
