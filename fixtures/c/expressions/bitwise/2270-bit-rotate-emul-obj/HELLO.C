int main(void) {
  unsigned int x = 0x1234;
  unsigned int r = (x << 4) | (x >> 12);
  return (int)r;
}
