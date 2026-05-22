struct Small { int x; int y; };
struct Small make(void) {
  struct Small s;
  s.x = 10;
  s.y = 20;
  return s;
}
