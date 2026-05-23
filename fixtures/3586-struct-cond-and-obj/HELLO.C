struct S { int x; int y; } s;

int both_set(void) {
  if (s.x && s.y) return 1;
  return 0;
}
