struct Four { int a; int b; int c; int d; };

struct Four make(int v) {
  struct Four r;
  r.a = v;
  r.b = v;
  r.c = v;
  r.d = v;
  return r;
}
