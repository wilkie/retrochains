struct P { int x; int y; };
int extract(struct P **pp) {
  return (*pp)->x;
}
