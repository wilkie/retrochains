int main(void) {
  int x = 0x1234;
  int hi = (x >> 4) & 0x0F;
  return hi;
}
