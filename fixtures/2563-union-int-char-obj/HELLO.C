union U { int i; char c[2]; };
union U u;
int main(void) {
  u.i = 0x1234;
  return u.c[0] + u.c[1];
}
