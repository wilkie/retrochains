int main(void) {
  char s[3];
  char *p;
  s[0] = 'a';
  s[1] = 'b';
  s[2] = 0;
  p = s;
  p++;
  return *p;
}
