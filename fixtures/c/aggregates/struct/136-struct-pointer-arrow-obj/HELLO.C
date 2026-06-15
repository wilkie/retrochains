struct s { int x; };
int main(void) {
  struct s a;
  struct s *p = &a;
  p->x = 7;
  return p->x;
}
