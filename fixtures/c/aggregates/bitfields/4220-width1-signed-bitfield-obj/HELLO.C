struct S {
  int sign : 1;
  unsigned rest : 3;
};
int main(void) {
  struct S s;
  s.sign = 1;
  s.rest = 5;
  return s.sign + (int)s.rest;
}
