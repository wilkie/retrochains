struct Signed { int x : 4; int y : 4; };
int main(void) {
  struct Signed s;
  s.x = -3;
  s.y = 5;
  return s.x + s.y;
}
