int my_strlen(char *s) {
  int n;
  n = 0;
  while (*s) {
    n = n + 1;
    s = s + 1;
  }
  return n;
}
int main(void) {
  char buf[10];
  buf[0] = 'h';
  buf[1] = 'i';
  buf[2] = '!';
  buf[3] = 0;
  return my_strlen(buf);
}
