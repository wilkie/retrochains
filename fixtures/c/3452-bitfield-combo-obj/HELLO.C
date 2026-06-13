struct F {
  unsigned a : 4;
  unsigned b : 4;
} f;

int both(void) {
  if (f.a & f.b) return 1;
  return 0;
}
