struct Big { int a; int b; char c; };
struct Big make(void) {
  struct Big v;
  v.a = 1;
  v.b = 2;
  v.c = 'Z';
  return v;
}
