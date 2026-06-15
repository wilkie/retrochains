struct Flags {
  unsigned a : 4;
  unsigned b : 4;
} s;

unsigned get_b(void) {
  return s.b;
}
