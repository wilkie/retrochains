struct P { int x; int y; int z; };
int main(void) {
  struct P p = {10, 20, 30};
  return p.x + p.y + p.z;
}
