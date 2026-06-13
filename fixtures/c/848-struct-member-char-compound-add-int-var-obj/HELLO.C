struct S { char c; };
struct S s;
int main() {
  int y;
  s.c = 10;
  y = 5;
  s.c += y;
  return 0;
}
