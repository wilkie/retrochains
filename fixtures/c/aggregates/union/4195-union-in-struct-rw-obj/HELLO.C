struct S {
  int tag;
  union {
    int i;
    char c[2];
  } u;
};
struct S g;
int main()
{
  g.tag = 1;
  g.u.i = 0x0102;
  return g.tag + g.u.c[0] + g.u.c[1];
}
