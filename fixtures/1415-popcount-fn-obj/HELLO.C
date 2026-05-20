int popcount(int x) {
  int c = 0;
  while (x) {
    if (x & 1) c++;
    x >>= 1;
  }
  return c;
}
int main(void) {
  return popcount(0x55);
}
