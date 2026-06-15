struct Mixed { char a; int b; char c; int d; };
struct Mixed g = { 1, 2, 3, 4 };
int main(void) {
  return g.b + g.d;
}
