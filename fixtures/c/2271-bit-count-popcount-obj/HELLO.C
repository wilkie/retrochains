int popcnt(unsigned int x) {
  int n = 0;
  while (x) { n += x & 1; x >>= 1; }
  return n;
}
int main(void) {
  return popcnt(0xFF) + popcnt(0x55);
}
