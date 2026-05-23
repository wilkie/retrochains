struct S {
  int x : 4;
  int y : 4;
} s;

int get_x(void) {
  return s.x;
}
