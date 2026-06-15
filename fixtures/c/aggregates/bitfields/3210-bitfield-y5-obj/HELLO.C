struct B { unsigned int x : 3; unsigned int y : 5; };
struct B b;
int get_y(void) {
  return b.y;
}
