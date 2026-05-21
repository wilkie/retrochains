int main(void) {
  unsigned int x = 0x6789;
  unsigned int nibble2 = (x >> 8) & 0x0F;
  return (int)nibble2;
}
