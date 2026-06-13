int main(void) {
  unsigned int x = 0xABCD;
  unsigned int r = x & 0xFFFF;
  return (int)r;
}
