long a[3];
long *p;
int main() {
  p = a;
  a[1] = 7L;
  p[1] <<= 1;
  return 0;
}
