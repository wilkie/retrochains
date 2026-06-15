long make_long(int hi, int lo) {
  long x;
  x = ((long)hi << 16) | (long)(unsigned int)lo;
  return x;
}
int main(void) {
  long y = make_long(0x12, 0x34);
  return (int)(y & 0xFFFF);
}
