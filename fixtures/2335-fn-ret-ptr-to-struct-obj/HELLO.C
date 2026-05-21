struct P { int x; int y; };
struct P *get(void) {
  static struct P p = {3, 4};
  return &p;
}
int main(void) {
  struct P *q = get();
  return q->x + q->y;
}
