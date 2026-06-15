struct S { char c; };
struct S s;
int main(void) {
  s.c = 'A';
  if (s.c == 'A') return 1;
  return 0;
}
