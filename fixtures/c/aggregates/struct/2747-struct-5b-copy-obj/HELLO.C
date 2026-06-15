struct Five { int a; int b; char c; };
int main(void) {
  struct Five s1;
  struct Five s2;
  s1.a = 1;
  s1.b = 2;
  s1.c = 'X';
  s2 = s1;
  return s2.b;
}
