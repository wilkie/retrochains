struct s { int x; };
int main(void) {
  struct s a;
  struct s *p = &a;
  p->x = 10;
  p->x += 5;
  return p->x;
}
