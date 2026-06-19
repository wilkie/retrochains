struct P { int x; int y; };
int rd_x(struct P *p) { return p[1].x; }
int rd_y(struct P *p) { return p[2].y; }
int rd_x0(struct P *p) { return p[0].x; }
void wr_y(struct P *p, int v) { p[2].y = v; }
void wr_x0(struct P *p, int v) { p[0].x = v; }
int main(void) {
  return 0;
}
