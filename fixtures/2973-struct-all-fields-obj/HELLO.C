struct M { int a; int b; int c; };
struct M m = { 10, 20, 30 };
int main(void) {
  return m.a + m.b + m.c;
}
