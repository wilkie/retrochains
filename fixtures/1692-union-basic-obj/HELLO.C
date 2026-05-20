union U {
  int i;
  char c[2];
};
int main(void) {
  union U u;
  u.i = 0x1234;
  return (int)u.c[0];
}
