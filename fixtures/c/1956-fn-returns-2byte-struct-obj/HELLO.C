struct One { int x; };
struct One make_one(int v) {
  struct One o;
  o.x = v + 1;
  return o;
}
int main(void) {
  struct One s = make_one(41);
  return s.x;
}
