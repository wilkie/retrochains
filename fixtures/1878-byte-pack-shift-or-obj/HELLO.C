int main(void) {
  unsigned int hi = 0xAB;
  unsigned int lo = 0xCD;
  unsigned int packed = (hi << 8) | lo;
  return (int)packed;
}
