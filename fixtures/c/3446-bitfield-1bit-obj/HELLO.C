struct Flag {
  unsigned a : 1;
  unsigned b : 1;
  unsigned c : 1;
} fl;

unsigned get_b(void) {
  return fl.b;
}
