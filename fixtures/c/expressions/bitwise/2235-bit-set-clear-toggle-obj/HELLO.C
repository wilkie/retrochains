int main(void) {
  unsigned int x = 0xF0F0;
  x |= 0x000F;
  x &= 0xFFF0;
  x ^= 0xAAAA;
  return (int)x;
}
