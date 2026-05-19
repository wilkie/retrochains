int main(void) {
  char a[3];
  char *p;
  p = a;
  p[1] = 'B';
  return a[1];
}
