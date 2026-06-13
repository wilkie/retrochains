int main(void) {
  int x = 0x100;
  x |= 0xf;
  return x >> 4;
}
