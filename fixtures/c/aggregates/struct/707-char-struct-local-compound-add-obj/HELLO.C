struct S { int i; char c; };
int main() {
  struct S s;
  s.i = 0;
  s.c = 1;
  s.c += 5;
  return s.c;
}
