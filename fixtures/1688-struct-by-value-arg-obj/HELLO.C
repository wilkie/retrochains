struct P { int x; int y; };
int sum(struct P p) {
  return p.x + p.y;
}
int main(void) {
  struct P q;
  q.x = 10;
  q.y = 20;
  return sum(q);
}
