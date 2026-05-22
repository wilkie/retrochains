struct Tiny { char c; };
struct Tiny make(void) {
  struct Tiny t;
  t.c = 'A';
  return t;
}
