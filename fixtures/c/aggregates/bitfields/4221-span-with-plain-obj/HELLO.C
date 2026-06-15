struct S {
  unsigned int lo : 6;
  unsigned int hi : 6;
  char tag;
};
int main(void)
{
  struct S s;
  s.lo = 50;
  s.hi = 40;
  s.tag = 7;
  return (int)s.lo + (int)s.hi + s.tag;
}
