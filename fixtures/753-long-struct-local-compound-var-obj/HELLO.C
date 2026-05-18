struct S { long x; };
int main() {
  struct S s;
  long h;
  s.x = 100;
  h = 50;
  s.x += h;
  return 0;
}
