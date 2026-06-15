int main(void) {
  int x = -5;
  long y = (long)x;
  return (int)y + (int)(y >> 16);
}
