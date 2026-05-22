struct Big { long v; };
struct Big make(void) {
  struct Big b;
  b.v = 0x12345678L;
  return b;
}
