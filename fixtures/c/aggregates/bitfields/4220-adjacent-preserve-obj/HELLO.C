struct S { unsigned int a : 4; unsigned int b : 4; };
int main(void)
{
  struct S s;
  s.a = 0xF;
  s.b = 0xF;
  s.a = 2;
  return s.a * 100 + s.b;
}
