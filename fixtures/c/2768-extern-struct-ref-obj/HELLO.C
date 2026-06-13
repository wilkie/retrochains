struct P { int x; int y; };
extern struct P shared;
int get_x(void) {
  return shared.x;
}
