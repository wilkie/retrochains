struct S { int x; int y; } s;

int via_addr(void) {
  return (&s)->y;
}
