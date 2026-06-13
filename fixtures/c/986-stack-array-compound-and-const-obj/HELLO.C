int main(void) {
  int a[3];
  a[0] = 0;
  a[1] = 0xFF;
  a[2] = 0;
  a[1] &= 0x0F;
  return a[1];
}
