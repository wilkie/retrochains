struct S { int x; };
int main(void) {
  struct S s = {42};
  return s.x;
}
