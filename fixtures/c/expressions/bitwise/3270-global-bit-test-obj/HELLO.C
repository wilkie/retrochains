int g;
int high_bit(void) {
  if (g & 0x80) return 1;
  return 0;
}
