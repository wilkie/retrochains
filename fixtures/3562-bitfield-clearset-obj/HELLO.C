struct F {
  unsigned a : 4;
  unsigned b : 4;
} f;

void set_a(unsigned v) {
  f.a = (f.a & 0) | (v & 0x0F);
}
