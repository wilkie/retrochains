struct Five { int a; int b; char c; };
struct Five make(void) {
  struct Five f;
  f.a = 1;
  f.b = 2;
  f.c = 'X';
  return f;
}
