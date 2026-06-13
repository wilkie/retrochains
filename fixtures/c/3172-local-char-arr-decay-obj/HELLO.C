int strlen_one(char *s);
int test(void) {
  char buf[4];
  buf[0] = 'A';
  buf[1] = 'B';
  buf[2] = 'C';
  buf[3] = 0;
  return strlen_one(buf);
}
