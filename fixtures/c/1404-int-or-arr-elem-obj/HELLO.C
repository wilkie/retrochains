int b[2] = {0x0a, 0x05};
int main(void) {
  int a = 0xf0;
  a |= b[0];
  return a;
}
