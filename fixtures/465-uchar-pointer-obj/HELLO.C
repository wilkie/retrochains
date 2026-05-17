unsigned char buf[3];
int g;
int main(void) {
  unsigned char *p;
  p = buf;
  *p = 200;
  g = *p + 1;
  return 0;
}
