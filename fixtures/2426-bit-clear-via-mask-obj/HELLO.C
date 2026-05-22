int main(void) {
  unsigned int x;
  x = 0xFF;
  x &= ~(1 << 3);
  return (int)x;
}
