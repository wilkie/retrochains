struct S { char c; };
int main(void) {
  struct S s;
  char b;
  s.c = 'Z';
  b = s.c;
  return b;
}
