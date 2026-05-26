struct Pair { int a; int b; };

struct Pair make(int a, int b) {
  struct Pair r;
  r.a = a;
  r.b = b;
  return r;
}
