struct P { int x; int y; };
void set_xy(struct P *p, int x, int y) {
  p->x = x;
  p->y = y;
}
int main(void) {
  struct P q;
  set_xy(&q, 10, 20);
  return q.x + q.y;
}
