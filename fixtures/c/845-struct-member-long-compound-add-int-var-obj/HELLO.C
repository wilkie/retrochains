struct S { long l; };
struct S s;
int main() {
  int y;
  s.l = 100;
  y = 5;
  s.l += y;
  return 0;
}
