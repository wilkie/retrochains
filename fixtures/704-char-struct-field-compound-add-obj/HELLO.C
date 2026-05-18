struct S { char c; int i; };
struct S g;
int main() {
  g.c += 5;
  return g.c;
}
