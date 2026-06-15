int main(void) {
  unsigned int x;
  int k;
  x = 0x10;
  k = 5;
  x |= (1 << k);
  return (int)x;
}
