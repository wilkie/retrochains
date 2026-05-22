struct Quad { char a; char b; char c; char d; };
struct Quad make(void) {
  struct Quad q;
  q.a = 'A';
  q.b = 'B';
  q.c = 'C';
  q.d = 'D';
  return q;
}
