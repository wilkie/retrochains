struct S { signed int flag : 1; signed int rest : 7; };
int main(void)
{
  struct S s;
  s.flag = 1;
  s.rest = 3;
  return s.flag * 10 + s.rest;
}
