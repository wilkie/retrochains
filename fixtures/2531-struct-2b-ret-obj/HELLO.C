struct Pair { char a; char b; };
struct Pair make(void) {
  struct Pair p;
  p.a = 'X';
  p.b = 'Y';
  return p;
}
