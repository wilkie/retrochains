struct S { char c; };
int main(void) {
  struct S s;
  s.c = 'Z';
  return s.c;
}
