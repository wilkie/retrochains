union U {
  char c;
  long l;
};
union U u;
int main(void) {
  u.l = 0x12345678L;
  return u.c;
}
