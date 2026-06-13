int main(void) {
  int x = 0x42;
  int y = (x >> 4) & 0xf;
  return y;
}
