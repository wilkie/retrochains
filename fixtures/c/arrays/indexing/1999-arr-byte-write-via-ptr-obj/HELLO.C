int main(void) {
  char a[4];
  char *p = a;
  *p++ = 'A';
  *p++ = 'B';
  *p++ = 'C';
  *p = '\0';
  return a[0] + a[1] + a[2];
}
