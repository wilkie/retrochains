struct Three { int x; char c; };
struct Three make(void) {
  struct Three s;
  s.x = 100;
  s.c = 'Z';
  return s;
}
