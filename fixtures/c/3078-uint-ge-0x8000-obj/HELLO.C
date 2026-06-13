int high(unsigned int v) {
  if (v >= 0x8000) return 1;
  return 0;
}
