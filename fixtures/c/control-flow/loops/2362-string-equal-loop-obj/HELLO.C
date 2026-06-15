int main(void) {
  char a[4];
  char b[4];
  char *p;
  char *q;
  a[0] = 'X';
  a[1] = 'Y';
  a[2] = 'Z';
  a[3] = 0;
  b[0] = 'X';
  b[1] = 'Y';
  b[2] = 'Z';
  b[3] = 0;
  p = a;
  q = b;
  while (*p == *q && *p) {
    p++;
    q++;
  }
  return (int)(*p - *q);
}
