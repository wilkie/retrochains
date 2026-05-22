int main(void) {
  unsigned int x;
  x = 0xABCD;
  x = (x | 0xFF00) & 0xFFFE;
  x = x ^ 0x000F;
  return (int)x;
}
