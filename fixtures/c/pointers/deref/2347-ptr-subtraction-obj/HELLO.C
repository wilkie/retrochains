int main(void) {
  char s[6];
  char *p;
  s[0] = 'A';
  s[1] = 'B';
  s[2] = 'C';
  s[3] = 0;
  s[4] = 'X';
  s[5] = 'Y';
  p = s;
  while (*p) p++;
  return (int)(p - s);
}
