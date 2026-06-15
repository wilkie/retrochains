union U {
  int i;
  unsigned char b[2];
};
union U u;
int main()
{
  int r;
  u.i = 0x1234;
  r = (u.b[1] << 8) | u.b[0];
  return r - u.i;
}
