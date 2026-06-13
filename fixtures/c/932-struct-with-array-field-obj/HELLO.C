struct S {
  int n;
  int a[3];
};
struct S s;
int main(void) {
  s.n = 7;
  s.a[1] = 9;
  return s.n + s.a[1];
}
