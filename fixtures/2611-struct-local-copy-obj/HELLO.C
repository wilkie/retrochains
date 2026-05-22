struct P { int x; int y; };
int main(void) {
  struct P s1;
  struct P s2;
  s1.x = 7;
  s1.y = 9;
  s2 = s1;
  return s2.x + s2.y;
}
