struct S { int x; };
struct S g;
struct S *gp = &g;
int main(void) {
  gp->x = 42;
  return gp->x;
}
