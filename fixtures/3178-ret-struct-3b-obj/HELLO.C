struct Tri { int x; char c; };
struct Tri make(int a, char b) {
  struct Tri t;
  t.x = a;
  t.c = b;
  return t;
}
