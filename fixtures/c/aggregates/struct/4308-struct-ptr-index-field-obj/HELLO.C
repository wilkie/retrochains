struct P { int x; int y; };
int rd_x(struct P *p) { return p[1].x; }
int rd_y(struct P *p) { return p[2].y; }
int rd_x0(struct P *p) { return p[0].x; }
int main(void) {
  return 0;
}
