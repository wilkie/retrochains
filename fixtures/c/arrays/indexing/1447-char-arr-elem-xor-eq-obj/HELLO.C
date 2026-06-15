int main(void) {
  char a[2];
  a[0] = 0xff;
  a[1] = 0x0f;
  a[0] ^= a[1];
  return a[0];
}
