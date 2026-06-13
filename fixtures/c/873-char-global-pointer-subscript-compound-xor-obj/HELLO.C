char a[3];
char *p;
int main() {
  int y;
  p = a;
  y = 7;
  p[1] ^= y;
  return 0;
}
