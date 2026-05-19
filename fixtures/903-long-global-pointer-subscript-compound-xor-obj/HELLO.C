long a[3];
long *p;
int main() {
  p = a;
  a[1] = 0xFFL;
  p[1] ^= 0xFL;
  return 0;
}
