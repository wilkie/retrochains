struct S { char c; int i; };
struct S g;
int main() {
  struct S *p;
  p = &g;
  p->c += 5;
  return p->c;
}
