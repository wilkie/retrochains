struct P { int a; int b; };
int g;
int use(struct P p) {
  g = p.a;
  return p.b;
}
