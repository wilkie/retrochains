int main(void) {
  unsigned int x;
  x = 0xABCD;
  return (int)((x >> 4) & 0xF);
}
