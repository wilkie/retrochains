struct S { unsigned a : 3; unsigned b : 3; unsigned c : 3; unsigned d : 3; unsigned e : 3; };
int main(void) {
  struct S s;
  s.a = 1; s.b = 2; s.c = 3; s.d = 4; s.e = 5;
  return (int)(s.a + s.b + s.c + s.d + s.e);
}
