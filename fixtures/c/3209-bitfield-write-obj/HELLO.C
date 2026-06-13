struct B { unsigned int x : 3; unsigned int y : 5; };
struct B b;
void set_x(void) {
  b.x = 5;
}
