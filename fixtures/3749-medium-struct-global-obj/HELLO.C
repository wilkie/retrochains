struct Q { int a; int b; };
struct Q g = { 5, 6 };
int main(void) {
  return g.a + g.b;
}
