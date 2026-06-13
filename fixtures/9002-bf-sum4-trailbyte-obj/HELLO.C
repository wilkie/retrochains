struct S { unsigned a : 4; unsigned b : 2; unsigned c : 2; unsigned d : 8; };
int main(void) {
  struct S s;
  s.a = 5; s.b = 1; s.c = 2; s.d = 100;
  return (int)(s.a + s.b + s.c + s.d);
}
