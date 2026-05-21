int main(void) {
  int x = 0x1234;
  x &= 0x0f0f;
  x |= 0xa0a0;
  x ^= 0x0500;
  return x;
}
