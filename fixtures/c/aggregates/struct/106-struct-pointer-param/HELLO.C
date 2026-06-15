struct s { int x; };
int get(struct s *p) {
  return p->x;
}
int main(void) {
  struct s a;
  a.x = 11;
  return get(&a);
}
