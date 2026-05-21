struct Pkt {
  unsigned int hi : 6;
  unsigned int span : 6;
  unsigned int lo : 4;
};
int main(void) {
  struct Pkt p;
  p.hi = 0x1F;
  p.span = 0x2A;
  p.lo = 0x7;
  return (int)(p.hi + p.span + p.lo);
}
