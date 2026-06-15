union U {
  int i;
  char c[2];
};
int hi_byte(p)
union U *p;
{
  return p->c[1];
}
int main()
{
  union U u;
  u.i = 0x7F00;
  return hi_byte(&u);
}
