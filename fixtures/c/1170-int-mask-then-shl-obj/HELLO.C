int main(void) {
  int a = 0x123;
  int x = (a & 0xff) << 4;
  return x;
}
