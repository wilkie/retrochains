struct S { int v[3]; };
struct S s = {{1, 2, 3}};
int main(void) {
  return s.v[0] + s.v[2];
}
