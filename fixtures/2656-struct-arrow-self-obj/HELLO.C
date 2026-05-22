struct P { int x; int y; };
void shift(struct P *p) {
  p->x = p->y + 1;
}
