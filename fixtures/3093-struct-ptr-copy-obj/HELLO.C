struct P { int x; int y; };
void copy(struct P *dst, struct P *src) {
  *dst = *src;
}
