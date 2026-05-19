int main(void) {
  int a = 0x12;
  int x = (a << 8) | 0xff;
  return x;
}
