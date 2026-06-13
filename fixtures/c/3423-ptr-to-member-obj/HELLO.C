struct S { int a; int b; } s;

int *grab_b(void) {
  return &s.b;
}
