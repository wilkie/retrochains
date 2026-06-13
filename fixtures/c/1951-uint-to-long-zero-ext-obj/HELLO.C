int main(void) {
  unsigned int x = 0x8000;
  long y = (long)x;
  return (int)(y >> 8);
}
