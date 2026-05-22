struct T3 { int a; char b; };
struct T3 make(void) {
  struct T3 t;
  t.a = 100;
  t.b = 'X';
  return t;
}
