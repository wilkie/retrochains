union U {
  struct { int lo; int hi; } parts;
  long whole;
};
int main(void) {
  union U u;
  u.parts.lo = 0x1234;
  u.parts.hi = 0x5678;
  return (int)(u.whole >> 16);
}
