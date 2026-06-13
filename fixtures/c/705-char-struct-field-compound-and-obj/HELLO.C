struct S { int i; char c; };
struct S g;
int main() {
  g.c &= 15;
  return g.c;
}
