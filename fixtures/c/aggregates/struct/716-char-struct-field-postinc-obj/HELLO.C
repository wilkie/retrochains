struct S { int i; char c; };
struct S g;
int main() {
  g.c++;
  return g.c;
}
