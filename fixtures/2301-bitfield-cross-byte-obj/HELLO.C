struct X { unsigned int lo : 6; unsigned int hi : 6; };
int main(void) {
  struct X x;
  x.lo = 0x3F;
  x.hi = 0x2A;
  return (int)(x.lo + x.hi);
}
