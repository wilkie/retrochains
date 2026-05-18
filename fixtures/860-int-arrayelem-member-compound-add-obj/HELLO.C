struct S { int x; };
struct S a[2];
int main() {
  int y;
  y = 7;
  a[1].x += y;
  return 0;
}
