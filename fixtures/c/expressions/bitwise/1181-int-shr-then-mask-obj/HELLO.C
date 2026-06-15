int main(void) {
  int a = 0x123;
  int x = (a >> 4) & 0xf;
  return x;
}
