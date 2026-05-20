int main(void) {
  char buf[3];
  char *p;
  p = buf;
  *p++ = 'A';
  *p++ = 'B';
  *p = 'C';
  return buf[1];
}
