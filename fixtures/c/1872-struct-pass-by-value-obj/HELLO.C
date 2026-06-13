struct P { int x; int y; };
int sum_p(struct P p) {
  return p.x + p.y;
}
int main(void) {
  struct P q;
  q.x = 3;
  q.y = 4;
  return sum_p(q);
}
