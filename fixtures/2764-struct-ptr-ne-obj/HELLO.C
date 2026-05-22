struct P { int x; };
int diff(struct P *a, struct P *b) {
  if (a != b) return 1;
  return 0;
}
