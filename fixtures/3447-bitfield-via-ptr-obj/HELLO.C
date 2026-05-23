struct F {
  unsigned a : 4;
  unsigned b : 4;
};

unsigned get_a(struct F *p) {
  return p->a;
}
