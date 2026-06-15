int main(void) {
  int a = 0xff;
  if (a > 0x80) return 1;
  return 0;
}
