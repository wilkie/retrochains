struct Six { int a; int b; int c; };
int main(void) {
  struct Six s1;
  struct Six s2;
  s1.a = 1;
  s1.b = 2;
  s1.c = 3;
  s2 = s1;
  return s2.b;
}
