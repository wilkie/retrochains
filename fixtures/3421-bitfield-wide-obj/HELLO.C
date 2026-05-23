struct W {
  unsigned lo : 8;
  unsigned hi : 8;
} w;

unsigned get_hi(void) {
  return w.hi;
}
