struct M { int x; char c; };
int main(void) {
  struct M m = {100, 'A'};
  return m.x + (int)m.c;
}
