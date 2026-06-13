struct S { int i; char c; };
struct S g;
int main() {
  char d;
  d = 3;
  g.c += d;
  return g.c;
}
