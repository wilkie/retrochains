struct Inner { int x; };
struct Outer { struct Inner inner; };
struct Outer o;
int main() {
  int y;
  y = 7;
  o.inner.x += y;
  return 0;
}
