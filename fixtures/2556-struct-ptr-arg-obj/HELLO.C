struct Big { int x; int y; int z; int w; };
int sum(struct Big *p) {
  return p->x + p->y + p->z + p->w;
}
