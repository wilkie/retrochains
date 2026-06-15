int main(void) {
  int x = 0x1234;
  char *p = (char *)&x;
  return *p;
}
